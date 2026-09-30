//! Loopback HTTP control plane for the Contabo music unit.
//!
//! Fullstack proxies Panel/API here. This process owns the only
//! `BotSupervisor` / decode → Opus → wire send loop. No Surreal.
//!
//! Optional shared-token auth lives in this HTTP layer.
//! `MUSIC_RUNTIME_TOKEN` (environment only) requires
//! `Authorization: Bearer <token>` on every route except `GET /health`.
//! When the variable is unset the listener must be loopback, or the
//! process refuses to start.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{delete, get, post};
use futures::stream::{Stream, StreamExt};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio_stream::wrappers::BroadcastStream;
use tracing::warn;

/// Environment variable that holds the shared bearer token.
///
/// Read at startup. Never a file, a database, or a response field.
/// Defined once in the audio crate. [`music_bot_audio::cpuset::music_command`]
/// and [`music_bot_audio::cpuset::std_music_command`] remove it from child
/// environments; this module does not spawn processes.
pub use music_bot_audio::cpuset::MUSIC_RUNTIME_TOKEN_ENV;

use crate::config::BotId;
use crate::runtime_api::{
    BugReportContextResponse, HealthResponse, ListResponse, MutateOp, SendRequest, SettingsRequest,
    SpawnRequest, SpawnResponse, StoreOp, WireError, store_err_to_wire,
};
use crate::store::{LibraryEntryId, PlaylistName, StoreError, TrackId};
use crate::supervisor::BotSupervisor;

#[derive(Clone)]
pub struct RuntimeState {
    pub supervisor: Arc<BotSupervisor>,
    pub yt_cookie: Arc<RwLock<Option<PathBuf>>>,
    pub yt_api_key: Arc<RwLock<Option<String>>>,
}

impl RuntimeState {
    pub fn new() -> Self {
        Self {
            supervisor: Arc::new(BotSupervisor::new()),
            yt_cookie: Arc::new(RwLock::new(None)),
            yt_api_key: Arc::new(RwLock::new(None)),
        }
    }
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self::new()
    }
}

/// How the control API authenticates callers.
///
/// Only the SHA-256 digest is stored. The raw token is dropped after
/// parse. `Debug` redacts the digest. [`ControlAuth`] does not spawn
/// processes and does not strip child environments — that is
/// [`music_bot_audio::cpuset::music_command`] /
/// [`music_bot_audio::cpuset::std_music_command`].
#[derive(Clone, Copy)]
pub enum ControlAuth {
    /// No bearer check. Valid only when the listener is loopback.
    Open,
    /// SHA-256 of the trimmed `MUSIC_RUNTIME_TOKEN`.
    Bearer([u8; 32]),
}

impl std::fmt::Debug for ControlAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open => f.write_str("ControlAuth::Open"),
            Self::Bearer(_) => f.write_str("ControlAuth::Bearer([redacted])"),
        }
    }
}

impl ControlAuth {
    pub const fn open() -> Self {
        Self::Open
    }

    pub fn is_open(&self) -> bool {
        matches!(self, Self::Open)
    }

    /// Read [`MUSIC_RUNTIME_TOKEN_ENV`]. Missing, empty, and
    /// whitespace-only values are [`ControlAuth::Open`]. A non-UTF-8
    /// value refuses to start on every bind address.
    pub fn from_env() -> Result<Self, ControlAuthError> {
        match std::env::var_os(MUSIC_RUNTIME_TOKEN_ENV) {
            None => Ok(Self::Open),
            Some(value) => Self::from_os_value(Some(value.as_os_str())),
        }
    }

    /// `None` is open. UTF-8 values follow [`Self::parse`]. A non-UTF-8
    /// `OsStr` is [`ControlAuthError`] and does not echo the bytes.
    pub fn from_os_value(raw: Option<&std::ffi::OsStr>) -> Result<Self, ControlAuthError> {
        let Some(raw) = raw else {
            return Ok(Self::Open);
        };
        match raw.to_str() {
            Some(text) => Ok(Self::parse(Some(text))),
            None => Err(ControlAuthError),
        }
    }

    /// `None`, `""`, and whitespace-only are open. Any other value
    /// enables bearer auth. Surrounding whitespace is ignored. Only
    /// the SHA-256 digest is stored.
    pub fn parse(raw: Option<&str>) -> Self {
        let Some(raw) = raw else {
            return Self::Open;
        };
        let token = raw.trim();
        if token.is_empty() {
            Self::Open
        } else {
            Self::Bearer(sha256(token.as_bytes()))
        }
    }

    /// Refuse a non-loopback bind when auth is disabled, and refuse an
    /// unspecified bind when auth is enabled unless the wildcard
    /// override is passed to [`decide_control_bind`].
    ///
    /// This path does not read `MUSIC_RUNTIME_ALLOW_WILDCARD_BIND`.
    /// Startup uses [`decide_control_bind`] so the override stays explicit.
    pub fn ensure_bind_allowed(
        &self,
        listen: SocketAddr,
    ) -> Result<BindDecision, ControlBindError> {
        decide_control_bind(listen, !self.is_open(), false)
    }
}

/// `MUSIC_RUNTIME_ALLOW_WILDCARD_BIND`. Exactly `1` or `true`
/// (ASCII case-insensitive) allows an unspecified bind when a token
/// is set. Anything else, including unset, is off. Discouraged.
pub const MUSIC_RUNTIME_ALLOW_WILDCARD_BIND_ENV: &str = "MUSIC_RUNTIME_ALLOW_WILDCARD_BIND";

/// Whether startup should continue, and whether it must warn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindDecision {
    /// One warning: the listener is on every interface and must be
    /// firewalled to the private tunnel.
    pub warn_wildcard: bool,
    /// One warning: the listener is a public address. Bind the
    /// WireGuard or private address and firewall `:3002` to the tunnel.
    pub warn_public: bool,
}

/// `Some("1")` and `Some("true")` (any ASCII case) are on.
/// Every other string, including surrounding whitespace, is off.
pub fn parse_allow_wildcard_bind(raw: Option<&str>) -> bool {
    match raw {
        Some(value) => value.eq_ignore_ascii_case("1") || value.eq_ignore_ascii_case("true"),
        None => false,
    }
}

fn is_wildcard_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_unspecified(),
        std::net::IpAddr::V6(v6) => {
            v6.is_unspecified() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_unspecified())
        }
    }
}

/// 100.64.0.0/10 (RFC 6598). Some tunnels use this range.
fn is_cgnat_v4(ip: std::net::Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 100 && (octets[1] & 0xc0) == 0x40
}

/// A specific address that is not loopback, not private, and not
/// link-local. IPv4-mapped IPv6 is classified by the embedded v4.
///
/// Private is IPv4 RFC 1918, IPv4 CGNAT 100.64.0.0/10, or IPv6 ULA
/// `fc00::/7`. Link-local is 169.254.0.0/16 or `fe80::/10`.
fn is_public_bind_addr(ip: std::net::IpAddr) -> bool {
    let ip = match ip {
        std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().map(std::net::IpAddr::V4).unwrap_or(ip),
        other => other,
    };
    if ip.is_loopback() {
        return false;
    }
    match ip {
        std::net::IpAddr::V4(v4) => !(v4.is_private() || v4.is_link_local() || is_cgnat_v4(v4)),
        std::net::IpAddr::V6(v6) => !(v6.is_unique_local() || v6.is_unicast_link_local()),
    }
}

/// Pure startup rule for the control listener.
///
/// `token_present` is true when a bearer token is configured.
/// `allow_wildcard` is the parsed override. Neither value is read
/// from the process environment here.
///
/// - No token and loopback: allow, no warning.
/// - No token and anything else: refuse.
/// - Token and `0.0.0.0`, `::`, or IPv4-mapped unspecified: refuse
///   unless `allow_wildcard`, in which case allow and warn.
/// - Token and a specific public address: allow, and set
///   [`BindDecision::warn_public`].
/// - Token and loopback, private, or link-local: allow, no warning.
pub fn decide_control_bind(
    listen: SocketAddr,
    token_present: bool,
    allow_wildcard: bool,
) -> Result<BindDecision, ControlBindError> {
    if !token_present {
        return if listen.ip().is_loopback() {
            Ok(BindDecision {
                warn_wildcard: false,
                warn_public: false,
            })
        } else {
            Err(ControlBindError::Unauthenticated { listen })
        };
    }
    if is_wildcard_ip(listen.ip()) {
        return if allow_wildcard {
            Ok(BindDecision {
                warn_wildcard: true,
                warn_public: false,
            })
        } else {
            Err(ControlBindError::Wildcard { listen })
        };
    }
    Ok(BindDecision {
        warn_wildcard: false,
        warn_public: is_public_bind_addr(listen.ip()),
    })
}

/// `MUSIC_RUNTIME_TOKEN` is set to bytes that are not UTF-8.
///
/// The process refuses to start on every bind address. The value is
/// not included in the message.
#[derive(Debug, thiserror::Error)]
#[error(
    "MUSIC_RUNTIME_TOKEN is set but is not valid UTF-8. Refusing to start regardless of bind address. Set a UTF-8 token or unset MUSIC_RUNTIME_TOKEN"
)]
pub struct ControlAuthError;

/// Startup failure for an unsafe control-API bind.
#[derive(Debug, thiserror::Error)]
pub enum ControlBindError {
    /// No token, and the listener is not loopback.
    #[error(
        "MUSIC_RUNTIME_TOKEN is unset and the music control API is bound to {listen}, which is not a loopback address. Refusing to start. Set MUSIC_RUNTIME_TOKEN or bind to 127.0.0.1 / ::1 so the control API is not exposed without authentication"
    )]
    Unauthenticated { listen: SocketAddr },
    /// Token is set, but the listener is `0.0.0.0`, `::`, or an
    /// IPv4-mapped unspecified address, and the override is off.
    #[error(
        "MUSIC_RUNTIME_TOKEN is set but the music control API is bound to {listen}, which listens on every interface. Refusing to start. Bind the WireGuard or other private address instead. MUSIC_RUNTIME_ALLOW_WILDCARD_BIND=1 overrides this and is discouraged"
    )]
    Wildcard { listen: SocketAddr },
}

/// Control plane with authentication disabled.
///
/// Production `ts6-manager-music` uses [`router_with_auth`] after
/// [`ControlAuth::ensure_bind_allowed`]. In-process callers on loopback
/// keep this open router.
pub fn router(state: RuntimeState) -> Router {
    router_with_auth(state, ControlAuth::open())
}

/// Same routes as [`router`], with bearer auth on every route except
/// `GET /health` when `auth` is [`ControlAuth::Bearer`].
pub fn router_with_auth(state: RuntimeState, auth: ControlAuth) -> Router {
    let mut app = Router::new()
        .route("/v1/bots", get(list_bots).post(spawn_bot))
        .route("/v1/bots/{id}", delete(shutdown_bot))
        .route("/v1/bots/{id}/command", post(send_command))
        .route("/v1/bots/{id}/events", get(events_sse))
        .route("/v1/settings", post(update_settings))
        .route("/v1/bug-report-context", get(bug_report_context))
        .route("/v1/mutate", post(mutate))
        .route("/v1/store", post(store_op));
    if let ControlAuth::Bearer(expected) = auth {
        app = app.route_layer(middleware::from_fn(move |req, next| async move {
            require_bearer(expected, req, next).await
        }));
    }
    // Added after `route_layer`, so `/health` is not authenticated.
    app.route("/health", get(health)).with_state(state)
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// Fixed-length compare of SHA-256 digests via `subtle::ConstantTimeEq`.
///
/// The configured secret is already a digest; only the presented header
/// is hashed here. Both sides are 32 bytes, so the compare does not
/// return on the first differing token byte or on a length mismatch of
/// the raw bearer value.
fn token_eq(expected_sha256: [u8; 32], presented: &[u8]) -> bool {
    bool::from(sha256(presented).ct_eq(&expected_sha256))
}

fn bearer_credential(value: &HeaderValue) -> Option<&[u8]> {
    const SCHEME: &[u8] = b"bearer ";
    let bytes = value.as_bytes();
    if bytes.len() <= SCHEME.len() {
        return None;
    }
    let (scheme, rest) = bytes.split_at(SCHEME.len());
    if !scheme.eq_ignore_ascii_case(SCHEME) {
        return None;
    }
    Some(rest)
}

async fn require_bearer(expected: [u8; 32], req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(bearer_credential)
        .is_some_and(|presented| token_eq(expected, presented));
    if ok {
        next.run(req).await
    } else {
        unauthorized()
    }
}

fn unauthorized() -> Response {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(axum::http::header::WWW_AUTHENTICATE, "Bearer")
        .body(axum::body::Body::empty())
        .expect("empty 401")
}

async fn health(State(state): State<RuntimeState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".into(),
        bots: state.supervisor.list().await.len(),
        send_loop: "owned".into(),
    })
}

async fn list_bots(State(state): State<RuntimeState>) -> Json<ListResponse> {
    Json(ListResponse {
        bots: state.supervisor.list().await,
    })
}

async fn spawn_bot(
    State(state): State<RuntimeState>,
    Json(req): Json<SpawnRequest>,
) -> Result<Json<SpawnResponse>, Response> {
    let id = if let Some(id) = req.id {
        state
            .supervisor
            .spawn_with_id(
                BotId(id),
                req.config,
                Arc::clone(&state.yt_cookie),
                Arc::clone(&state.yt_api_key),
            )
            .await
    } else {
        state
            .supervisor
            .spawn(
                req.config,
                Arc::clone(&state.yt_cookie),
                Arc::clone(&state.yt_api_key),
            )
            .await
    };
    Ok(Json(SpawnResponse { id }))
}

async fn shutdown_bot(
    State(state): State<RuntimeState>,
    Path(id): Path<u64>,
) -> Result<StatusCode, Response> {
    state
        .supervisor
        .shutdown_bot(BotId(id))
        .await
        .map_err(send_err)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn send_command(
    State(state): State<RuntimeState>,
    Path(id): Path<u64>,
    Json(req): Json<SendRequest>,
) -> Result<StatusCode, Response> {
    state
        .supervisor
        .send(BotId(id), req.command)
        .await
        .map_err(send_err)?;
    Ok(StatusCode::ACCEPTED)
}

async fn events_sse(
    State(state): State<RuntimeState>,
    Path(id): Path<u64>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Response> {
    let rx = state
        .supervisor
        .subscribe(BotId(id))
        .await
        .ok_or_else(|| status_err(StatusCode::NOT_FOUND, "bot not found"))?;
    let stream = BroadcastStream::new(rx).filter_map(|item| async move {
        match item {
            Ok(ev) => match serde_json::to_string(&ev) {
                Ok(json) => Some(Ok(Event::default().data(json))),
                Err(err) => {
                    warn!(?err, "failed to serialise BotEvent for music-runtime SSE");
                    None
                }
            },
            Err(_) => None,
        }
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

async fn update_settings(
    State(state): State<RuntimeState>,
    Json(req): Json<SettingsRequest>,
) -> StatusCode {
    if let Some(cookie) = req.yt_cookie {
        *state.yt_cookie.write().unwrap_or_else(|e| e.into_inner()) = cookie;
    }
    if let Some(key) = req.yt_api_key {
        *state.yt_api_key.write().unwrap_or_else(|e| e.into_inner()) = key;
    }
    StatusCode::NO_CONTENT
}

async fn bug_report_context() -> Json<BugReportContextResponse> {
    let snap = crate::bug_report::snapshot();
    Json(BugReportContextResponse {
        music_bot_latency: snap.music_bot_latency,
        log_tail: snap.log_tail,
    })
}

async fn mutate(
    State(state): State<RuntimeState>,
    Json(op): Json<MutateOp>,
) -> Result<Json<serde_json::Value>, Response> {
    let value = match op {
        MutateOp::PlaylistCreate { bot, name } => {
            state
                .supervisor
                .playlist_create(BotId(bot), PlaylistName(name))
                .await
                .map_err(store_err)?;
            serde_json::json!(null)
        }
        MutateOp::PlaylistRename { bot, old, new } => {
            state
                .supervisor
                .playlist_rename(BotId(bot), PlaylistName(old), PlaylistName(new))
                .await
                .map_err(store_err)?;
            serde_json::json!(null)
        }
        MutateOp::PlaylistDelete { bot, name } => {
            state
                .supervisor
                .playlist_delete(BotId(bot), PlaylistName(name))
                .await
                .map_err(store_err)?;
            serde_json::json!(null)
        }
        MutateOp::PlaylistAddTrack { bot, name, track } => {
            let stored = state
                .supervisor
                .playlist_add_track(BotId(bot), &PlaylistName(name), track)
                .await
                .map_err(store_err)?;
            serde_json::to_value(stored).unwrap_or(serde_json::Value::Null)
        }
        MutateOp::PlaylistRemoveTrack { bot, name, id } => {
            let changed = state
                .supervisor
                .playlist_remove_track(BotId(bot), &PlaylistName(name), TrackId(id))
                .await
                .map_err(store_err)?;
            serde_json::json!(changed)
        }
        MutateOp::PlaylistList { bot } => {
            let names = state
                .supervisor
                .playlist_list(BotId(bot))
                .await
                .map_err(store_err)?;
            serde_json::to_value(names).unwrap_or(serde_json::Value::Null)
        }
        MutateOp::PlaylistListTracks { bot, name } => {
            let tracks = state
                .supervisor
                .playlist_list_tracks(BotId(bot), &PlaylistName(name))
                .await
                .map_err(store_err)?;
            serde_json::to_value(tracks).unwrap_or(serde_json::Value::Null)
        }
        MutateOp::EnqueuePlaylist { bot, name } => {
            let tracks = state
                .supervisor
                .store()
                .enqueue_playlist(BotId(bot), &PlaylistName(name))
                .await
                .map_err(store_err)?;
            serde_json::to_value(tracks).unwrap_or(serde_json::Value::Null)
        }
        MutateOp::LibraryAdd { bot, entry } => {
            let stored = state
                .supervisor
                .library_add(BotId(bot), entry)
                .await
                .map_err(store_err)?;
            serde_json::to_value(stored).unwrap_or(serde_json::Value::Null)
        }
        MutateOp::LibraryRemove { bot, id } => {
            let changed = state
                .supervisor
                .library_remove(BotId(bot), LibraryEntryId(id))
                .await
                .map_err(store_err)?;
            serde_json::json!(changed)
        }
        MutateOp::LibraryLookup { bot, id } => {
            let entry = state
                .supervisor
                .library_lookup(BotId(bot), LibraryEntryId(id))
                .await
                .map_err(store_err)?;
            serde_json::to_value(entry).unwrap_or(serde_json::Value::Null)
        }
        MutateOp::LibraryList { bot, tag } => {
            let entries = state
                .supervisor
                .library_list(BotId(bot), tag.as_deref())
                .await
                .map_err(store_err)?;
            serde_json::to_value(entries).unwrap_or(serde_json::Value::Null)
        }
    };
    Ok(Json(value))
}

async fn store_op(
    State(state): State<RuntimeState>,
    Json(op): Json<StoreOp>,
) -> Result<Json<serde_json::Value>, Response> {
    let store = state.supervisor.store();
    let value = match op {
        StoreOp::QueueEnqueue { bot, track } => {
            let stored = store
                .queue_enqueue(BotId(bot), track)
                .await
                .map_err(store_err)?;
            serde_json::to_value(stored).unwrap_or(serde_json::Value::Null)
        }
        StoreOp::QueueDequeueHead { bot } => {
            let head = store
                .queue_dequeue_head(BotId(bot))
                .await
                .map_err(store_err)?;
            serde_json::to_value(head).unwrap_or(serde_json::Value::Null)
        }
        StoreOp::QueuePeek { bot } => {
            let q = store.queue_peek(BotId(bot)).await.map_err(store_err)?;
            serde_json::to_value(q).unwrap_or(serde_json::Value::Null)
        }
        StoreOp::QueueClear { bot } => {
            store.queue_clear(BotId(bot)).await.map_err(store_err)?;
            serde_json::json!(null)
        }
        StoreOp::QueueReorder { bot, order } => {
            store
                .queue_reorder(BotId(bot), order.into_iter().map(TrackId).collect())
                .await
                .map_err(store_err)?;
            serde_json::json!(null)
        }
        StoreOp::QueueRemove { bot, id } => {
            let changed = store
                .queue_remove(BotId(bot), TrackId(id))
                .await
                .map_err(store_err)?;
            serde_json::json!(changed)
        }
        StoreOp::QueueCurrent { bot } => {
            let cur = store.queue_current(BotId(bot)).await.map_err(store_err)?;
            serde_json::to_value(cur).unwrap_or(serde_json::Value::Null)
        }
        StoreOp::QueueSetHeadTitle { bot, title } => {
            let head = store
                .queue_set_head_title(BotId(bot), title)
                .await
                .map_err(store_err)?;
            serde_json::to_value(head).unwrap_or(serde_json::Value::Null)
        }
        StoreOp::PlaylistCreate { bot, name } => {
            store
                .playlist_create(BotId(bot), PlaylistName(name))
                .await
                .map_err(store_err)?;
            serde_json::json!(null)
        }
        StoreOp::PlaylistRename { bot, old, new } => {
            store
                .playlist_rename(BotId(bot), PlaylistName(old), PlaylistName(new))
                .await
                .map_err(store_err)?;
            serde_json::json!(null)
        }
        StoreOp::PlaylistDelete { bot, name } => {
            store
                .playlist_delete(BotId(bot), PlaylistName(name))
                .await
                .map_err(store_err)?;
            serde_json::json!(null)
        }
        StoreOp::PlaylistAddTrack { bot, name, track } => {
            let stored = store
                .playlist_add_track(BotId(bot), &PlaylistName(name), track)
                .await
                .map_err(store_err)?;
            serde_json::to_value(stored).unwrap_or(serde_json::Value::Null)
        }
        StoreOp::PlaylistRemoveTrack { bot, name, id } => {
            let changed = store
                .playlist_remove_track(BotId(bot), &PlaylistName(name), TrackId(id))
                .await
                .map_err(store_err)?;
            serde_json::json!(changed)
        }
        StoreOp::PlaylistListTracks { bot, name } => {
            let tracks = store
                .playlist_list_tracks(BotId(bot), &PlaylistName(name))
                .await
                .map_err(store_err)?;
            serde_json::to_value(tracks).unwrap_or(serde_json::Value::Null)
        }
        StoreOp::PlaylistList { bot } => {
            let names = store.playlist_list(BotId(bot)).await.map_err(store_err)?;
            serde_json::to_value(names).unwrap_or(serde_json::Value::Null)
        }
        StoreOp::EnqueuePlaylist { bot, name } => {
            let tracks = store
                .enqueue_playlist(BotId(bot), &PlaylistName(name))
                .await
                .map_err(store_err)?;
            serde_json::to_value(tracks).unwrap_or(serde_json::Value::Null)
        }
        StoreOp::LibraryAdd { bot, entry } => {
            let stored = store
                .library_add(BotId(bot), entry)
                .await
                .map_err(store_err)?;
            serde_json::to_value(stored).unwrap_or(serde_json::Value::Null)
        }
        StoreOp::LibraryRemove { bot, id } => {
            let changed = store
                .library_remove(BotId(bot), LibraryEntryId(id))
                .await
                .map_err(store_err)?;
            serde_json::json!(changed)
        }
        StoreOp::LibraryLookup { bot, id } => {
            let entry = store
                .library_lookup(BotId(bot), LibraryEntryId(id))
                .await
                .map_err(store_err)?;
            serde_json::to_value(entry).unwrap_or(serde_json::Value::Null)
        }
        StoreOp::LibraryList { bot, tag } => {
            let entries = store
                .library_list(BotId(bot), tag.as_deref())
                .await
                .map_err(store_err)?;
            serde_json::to_value(entries).unwrap_or(serde_json::Value::Null)
        }
    };
    Ok(Json(value))
}

fn send_err(err: crate::supervisor::SendError) -> Response {
    status_err(StatusCode::NOT_FOUND, &err.to_string())
}

fn store_err(err: StoreError) -> Response {
    let status = match &err {
        StoreError::PlaylistNotFound(_)
        | StoreError::TrackNotFound(_)
        | StoreError::LibraryEntryNotFound(_) => StatusCode::NOT_FOUND,
        StoreError::PlaylistExists(_) => StatusCode::CONFLICT,
        StoreError::ReorderMismatch { .. } => StatusCode::BAD_REQUEST,
        StoreError::Snapshot(_) | StoreError::Backend(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let body = serde_json::to_vec(&store_err_to_wire(&err)).unwrap_or_default();
    Response::builder()
        .status(status)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|_| status_err(status, &err.to_string()))
}

fn status_err(status: StatusCode, msg: &str) -> Response {
    let body = serde_json::to_vec(&WireError {
        error: msg.to_string(),
    })
    .unwrap_or_default();
    Response::builder()
        .status(status)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(status)
                .body(axum::body::Body::from(msg.to_string()))
                .expect("static error response")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::BotCommand;
    use crate::config::BotConfig;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn json<T: serde::de::DeserializeOwned>(resp: axum::http::Response<Body>) -> T {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).expect("json body")
    }

    #[tokio::test]
    async fn health_and_spawn_and_command_roundtrip() {
        let app = router(RuntimeState::new());
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let health: HealthResponse = json(resp).await;
        assert_eq!(health.send_loop, "owned");
        assert_eq!(health.bots, 0);

        let cfg = BotConfig::new(
            "unit",
            std::env::temp_dir().join("music-runtime-unit.identity"),
        )
        .with_auto_connect(false);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/bots")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&SpawnRequest {
                            config: cfg,
                            id: None,
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let spawned: SpawnResponse = json(resp).await;

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/v1/bots/{}/command", spawned.id.0))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&SendRequest {
                            command: BotCommand::Disconnect,
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/bots")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let list: ListResponse = json(resp).await;
        assert_eq!(list.bots.len(), 1);
        assert_eq!(list.bots[0].name, "unit");
    }

    fn addr(text: &str) -> SocketAddr {
        text.parse()
            .unwrap_or_else(|_| panic!("socket addr {text}"))
    }

    fn authed(method: &str, uri: &str, token: Option<&str>, body: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            builder = builder.header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"));
        }
        if body.is_some() {
            builder = builder.header(axum::http::header::CONTENT_TYPE, "application/json");
        }
        builder
            .body(match body {
                Some(json) => Body::from(json.to_string()),
                None => Body::empty(),
            })
            .expect("request")
    }

    async fn assert_empty_401(resp: axum::http::Response<Body>, token: &str) {
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            resp.headers()
                .get(axum::http::header::WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer")
        );
        let headers = format!("{:?}", resp.headers());
        assert!(
            !headers.contains(token),
            "401 headers must not echo the token: {headers}"
        );
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(
            bytes.is_empty(),
            "401 body must be empty, got {:?}",
            String::from_utf8_lossy(&bytes)
        );
    }

    const PROTECTED: &[(&str, &str, Option<&str>)] = &[
        ("GET", "/v1/bots", None),
        ("POST", "/v1/bots", Some("{}")),
        ("DELETE", "/v1/bots/1", None),
        ("POST", "/v1/bots/1/command", Some("{}")),
        ("GET", "/v1/bots/1/events", None),
        ("POST", "/v1/settings", Some("{}")),
        ("GET", "/v1/bug-report-context", None),
        (
            "POST",
            "/v1/mutate",
            Some(r#"{"op":"playlist_list","bot":1}"#),
        ),
        ("POST", "/v1/store", Some(r#"{"op":"queue_peek","bot":1}"#)),
    ];

    #[tokio::test]
    async fn authorized_requests_ok_and_health_stays_open() {
        let token = "unit-test-token";
        let auth = ControlAuth::parse(Some(token));
        let decision = decide_control_bind(addr("10.8.0.2:3002"), true, false).unwrap();
        assert!(!decision.warn_wildcard);
        assert!(!decision.warn_public);
        let app = router_with_auth(RuntimeState::new(), auth);

        let resp = app
            .clone()
            .oneshot(authed("GET", "/health", None, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let health: HealthResponse = json(resp).await;
        assert_eq!(health.status, "ok");

        let resp = app
            .clone()
            .oneshot(authed("GET", "/v1/bots", Some(token), None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/bots")
                    .header(axum::http::header::AUTHORIZATION, format!("bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let cfg = BotConfig::new(
            "authed",
            std::env::temp_dir().join("music-runtime-auth.identity"),
        )
        .with_auto_connect(false);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/bots")
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&SpawnRequest {
                            config: cfg,
                            id: None,
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let spawned: SpawnResponse = json(resp).await;

        let resp = app
            .clone()
            .oneshot(authed(
                "POST",
                &format!("/v1/bots/{}/command", spawned.id.0),
                Some(token),
                Some(
                    &serde_json::to_string(&SendRequest {
                        command: BotCommand::Disconnect,
                    })
                    .unwrap(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);

        let resp = app
            .clone()
            .oneshot(authed(
                "GET",
                &format!("/v1/bots/{}/events", spawned.id.0),
                Some(token),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let content_type = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            content_type.starts_with("text/event-stream"),
            "SSE content-type, got {content_type}"
        );

        let resp = app
            .clone()
            .oneshot(authed(
                "DELETE",
                &format!("/v1/bots/{}", spawned.id.0),
                Some(token),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let resp = app
            .clone()
            .oneshot(authed("POST", "/v1/settings", Some(token), Some("{}")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let resp = app
            .clone()
            .oneshot(authed("GET", "/v1/bug-report-context", Some(token), None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app
            .clone()
            .oneshot(authed(
                "POST",
                "/v1/mutate",
                Some(token),
                Some(&format!(
                    r#"{{"op":"playlist_list","bot":{}}}"#,
                    spawned.id.0
                )),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app
            .oneshot(authed(
                "POST",
                "/v1/store",
                Some(token),
                Some(&format!(r#"{{"op":"queue_peek","bot":{}}}"#, spawned.id.0)),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn missing_or_wrong_token_is_401_on_each_protected_route() {
        let token = "runtime-token-do-not-echo";
        let app = router_with_auth(RuntimeState::new(), ControlAuth::parse(Some(token)));

        for (method, uri, body) in PROTECTED {
            for presented in [None, Some("wrong-token"), Some("runtime-token-do-not-ech")] {
                let resp = app
                    .clone()
                    .oneshot(authed(method, uri, presented, *body))
                    .await
                    .unwrap();
                assert_empty_401(resp, token).await;
            }
        }

        // Auth runs before the handler: a wrong token is 401, not 404.
        let resp = app
            .clone()
            .oneshot(authed("GET", "/v1/bots/1/events", Some("nope"), None))
            .await
            .unwrap();
        assert_empty_401(resp, token).await;

        let resp = app
            .clone()
            .oneshot(authed("GET", "/v1/bots/1/events", Some(token), None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/bots")
                    .header(axum::http::header::AUTHORIZATION, format!("Basic {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_empty_401(resp, token).await;

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/bots?token={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_empty_401(resp, token).await;

        let resp = app
            .oneshot(authed("GET", "/health", None, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn unset_token_on_loopback_leaves_routes_open() {
        let auth = ControlAuth::parse(None);
        assert!(auth.is_open());
        assert!(auth.ensure_bind_allowed(addr("127.0.0.1:3002")).is_ok());
        assert!(auth.ensure_bind_allowed(addr("127.0.0.2:3002")).is_ok());
        assert!(auth.ensure_bind_allowed(addr("[::1]:3002")).is_ok());
        let app = router_with_auth(RuntimeState::new(), auth);
        let resp = app
            .oneshot(authed("GET", "/v1/bots", None, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn unset_token_on_non_loopback_is_a_startup_error() {
        let auth = ControlAuth::parse(None);
        for raw in [
            "0.0.0.0:3002",
            "[::]:3002",
            "10.1.2.3:3002",
            "[2001:db8::1]:3002",
            "[::ffff:127.0.0.1]:3002",
        ] {
            let err = auth.ensure_bind_allowed(addr(raw)).expect_err(raw);
            let msg = err.to_string();
            assert!(msg.contains("MUSIC_RUNTIME_TOKEN"), "{msg}");
            assert!(msg.contains("Refusing to start"), "{msg}");
            assert!(msg.contains("loopback"), "{msg}");
            assert!(!msg.contains("secret"), "{msg}");
        }
    }

    #[test]
    fn wildcard_bind_with_token_refuses_unless_overridden() {
        for raw in ["0.0.0.0:3002", "[::]:3002", "[::ffff:0.0.0.0]:3002"] {
            let err = decide_control_bind(addr(raw), true, false).expect_err(raw);
            let msg = err.to_string();
            assert!(msg.contains("Refusing to start"), "{msg}");
            assert!(msg.contains("WireGuard"), "{msg}");
            assert!(msg.contains("private"), "{msg}");
            let allowed = decide_control_bind(addr(raw), true, true).expect(raw);
            assert!(allowed.warn_wildcard, "{raw}");
        }
        let wireguard = decide_control_bind(addr("10.8.0.2:3002"), true, false).unwrap();
        assert!(!wireguard.warn_wildcard);
        assert!(!wireguard.warn_public);
        let loopback = decide_control_bind(addr("127.0.0.1:3002"), true, false).unwrap();
        assert!(!loopback.warn_wildcard);
        assert!(!loopback.warn_public);
        let open = decide_control_bind(addr("127.0.0.1:3002"), false, false).unwrap();
        assert!(!open.warn_wildcard);
        let unset = decide_control_bind(addr("0.0.0.0:3002"), false, true).expect_err("no token");
        assert!(unset.to_string().contains("loopback"));
    }

    #[test]
    fn public_bind_with_token_allows_with_warn() {
        for raw in [
            "203.0.113.5:3002",
            "[2001:db8::1]:3002",
            "[::ffff:203.0.113.5]:3002",
        ] {
            let decision = decide_control_bind(addr(raw), true, false).expect(raw);
            assert!(!decision.warn_wildcard, "{raw}");
            assert!(decision.warn_public, "{raw}");
        }
        for raw in [
            "10.8.0.2:3002",
            "172.20.0.1:3002",
            "192.168.1.2:3002",
            "100.64.0.1:3002",
            "[fd00::1]:3002",
            "169.254.1.1:3002",
            "[fe80::1]:3002",
        ] {
            let decision = decide_control_bind(addr(raw), true, false).expect(raw);
            assert!(!decision.warn_wildcard, "{raw}");
            assert!(!decision.warn_public, "{raw}");
        }
        let unset = decide_control_bind(addr("203.0.113.5:3002"), false, false)
            .expect_err("no token on a public address");
        assert!(unset.to_string().contains("Refusing to start"));
    }

    #[test]
    fn allow_wildcard_bind_override_parsing() {
        assert!(!parse_allow_wildcard_bind(None));
        assert!(!parse_allow_wildcard_bind(Some("")));
        assert!(!parse_allow_wildcard_bind(Some("0")));
        assert!(!parse_allow_wildcard_bind(Some("yes")));
        assert!(!parse_allow_wildcard_bind(Some("false")));
        assert!(!parse_allow_wildcard_bind(Some(" true")));
        assert!(!parse_allow_wildcard_bind(Some("1 ")));
        assert!(!parse_allow_wildcard_bind(Some("true ")));
        assert!(parse_allow_wildcard_bind(Some("1")));
        assert!(parse_allow_wildcard_bind(Some("true")));
        assert!(parse_allow_wildcard_bind(Some("TRUE")));
        assert!(parse_allow_wildcard_bind(Some("True")));
    }

    #[test]
    fn empty_or_whitespace_token_is_treated_as_unset() {
        for raw in ["", " ", "\t", "\n", " \t\r\n "] {
            let auth = ControlAuth::parse(Some(raw));
            assert!(auth.is_open(), "{raw:?}");
            assert!(
                auth.ensure_bind_allowed(addr("0.0.0.0:3002")).is_err(),
                "{raw:?} must refuse a non-loopback bind"
            );
            assert!(auth.ensure_bind_allowed(addr("127.0.0.1:3002")).is_ok());
            assert!(auth.ensure_bind_allowed(addr("[::1]:3002")).is_ok());
        }
        assert!(!ControlAuth::parse(Some("0")).is_open());
        assert!(!ControlAuth::parse(Some("  x  ")).is_open());
    }

    #[tokio::test]
    async fn whitespace_token_router_stays_open_on_loopback() {
        let auth = ControlAuth::parse(Some(" \t "));
        assert!(auth.ensure_bind_allowed(addr("127.0.0.1:3002")).is_ok());
        let app = router_with_auth(RuntimeState::new(), auth);
        let resp = app
            .oneshot(authed("GET", "/v1/bots", None, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn surrounding_whitespace_on_the_env_value_is_not_part_of_the_token() {
        let auth = ControlAuth::parse(Some("  unit-test-token  "));
        assert!(!auth.is_open());
        let app = router_with_auth(RuntimeState::new(), auth);
        let resp = app
            .clone()
            .oneshot(authed("GET", "/v1/bots", Some("unit-test-token"), None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = app
            .oneshot(authed("GET", "/v1/bots", Some("  unit-test-token  "), None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_token_refuses_to_start() {
        use std::os::unix::ffi::OsStrExt;
        let raw = std::ffi::OsStr::from_bytes(b"not-utf8-\xff-token");
        let err = ControlAuth::from_os_value(Some(raw)).expect_err("non-utf8 token");
        let msg = err.to_string();
        assert!(msg.contains("Refusing to start"), "{msg}");
        assert!(msg.to_lowercase().contains("utf-8"), "{msg}");
        assert!(!msg.contains("not-utf8"), "{msg}");
        assert!(!format!("{err:?}").contains("not-utf8"), "{err:?}");
        // The failure is the parse result itself, so a loopback bind
        // cannot turn a non-UTF-8 token into an open listener.
        assert!(
            ControlAuth::from_os_value(Some(std::ffi::OsStr::new("  \t  ")))
                .unwrap()
                .is_open()
        );
    }

    #[test]
    fn debug_output_does_not_include_the_token() {
        let t = "super-secret-runtime-token";
        let rendered = format!("{:?}", ControlAuth::parse(Some(t)));
        assert!(!rendered.contains(t), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
        let err = ControlAuth::parse(None)
            .ensure_bind_allowed(addr("192.0.2.10:3002"))
            .expect_err("non-loopback");
        assert!(!err.to_string().contains(t));
    }
}
