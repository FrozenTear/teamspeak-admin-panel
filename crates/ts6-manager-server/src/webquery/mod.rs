//! Spec Chapter 10 — outbound HTTP client to the per-server TS6 WebQuery API.
//!
//! Phase 1 (PURA-23) shipped the **read-only** subset for the §7.19 dashboard.
//! Phase 2 (PURA-68) extends this module with the full ServerQuery command
//! surface the FE needs for ops actions:
//!
//! - **Read** — `clientlist`/`clientinfo`/`clientdblist`/`clientdbinfo`,
//!   `channellist`/`channelinfo`/`channelclientlist`,
//!   `serverinfo`/`hostinfo`/`logview`,
//!   `channelclientpermlist`.
//! - **Write** — `clientkick`/`clientpoke`/`clientmove`/`clientedit` (used by
//!   the talker-flag helper), `banadd`/`bandel`/`bandelall`.
//!
//! Cross-cutting:
//!
//! - [`WebQueryClient`] per `server_connection` row, single keep-alive socket
//!   so each upstream registers exactly one ServerQuery slot (§10.1).
//! - [`WebQueryPool`] keyed by `server_connection.id`, lazy-creating clients
//!   on first call. Boot-time pre-population, `autoStart`, the 30s health
//!   probe, and the `refreshClient` lifecycle on `PUT/DELETE /servers` are
//!   Phase 2 follow-ups owned by a separate ticket.
//! - Spec §10.5/§10.6 envelope handling: non-zero `status.code` → typed
//!   [`WebQueryError::Upstream`] which the route layer maps to `502
//!   {error: "TeamSpeak API Error", code, details}` per §7.0.2. Code `1281`
//!   (`database_empty_result`) is opt-in coerced to an empty list by
//!   list-shaped reads via [`WebQueryClient::list_or_empty`].
//! - Tracing: every request runs inside a `webquery.request` span carrying
//!   `config_id`, `method`, `path`, and emits a structured log with
//!   `latency_ms` and outcome (`ok` / `upstream_err` / `transport`).
//!
//! Out of scope (separate issues): REST endpoints exposing these actions
//! (PURA-71), WS event fan-out (separate child), SSH-based control path.
//!
//! ServerQuery `key=value` escaping (§10.4) lives in [`escape`]; it is **not**
//! applied here because WebQuery's URL encoder already handles the wire-side
//! transform and the spec forbids double-escaping. The escape pair is
//! exposed for the future SSH bridge.

#![allow(dead_code)] // consumed by REST routes and future WEBQUERY callers.

pub mod dashboard;
pub mod escape;
pub mod models;
pub mod probe;
pub mod transport_class;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Client, Method, StatusCode};
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};
use tracing::Instrument;

use crate::crypto;
use crate::repos::server_connections::ServerConnection;

pub use models::{
    BanAddResponse, BanEntry, ChannelClientPerm, ChannelEntry, ChannelGroupClient,
    ChannelGroupEntry, ChannelGroupIdResponse, ChannelInfo, ClientDbEntry, ClientEntry, ClientInfo,
    ComplaintEntry, ConnectionInfo, GroupPermEntry, HostInfo, LogEntry, MessageDetail,
    MessageEntry, PermFindEntry, PermIdEntry, PermOverviewEntry, PermissionEntry,
    PrivilegeKeyAddResponse, PrivilegeKeyEntry, ServerGroupClient, ServerGroupEntry,
    ServerGroupIdResponse, ServerInfo, VersionInfo, VirtualServerEntry,
};
pub use transport_class::{
    ClassifiedTransport, WebQueryTransportKind, other_static as other_transport,
};

/// TS upstream code for `database_empty_result`. List reads opt into mapping
/// this to an empty `Vec` via [`WebQueryClient::list_or_empty`] per §10.6.
pub const DATABASE_EMPTY_RESULT: i64 = 1281;

/// Spec §10.2 — fixed 15s request timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Spec §10.2 — API key header name. Lowercase; reqwest normalises but the
/// header name is canonically `x-api-key`.
const API_KEY_HEADER: &str = "x-api-key";

/// Errors returned by [`WebQueryClient`]. The REST layer maps these to HTTP
/// status codes per spec §7.0.2.
#[derive(Debug, Error)]
pub enum WebQueryError {
    /// TS upstream returned a non-zero `status.code`. `502 {error: "TeamSpeak
    /// API Error", code, details}` per §7.0.2.
    #[error("TS upstream error {code}: {message}")]
    Upstream { code: i64, message: String },

    /// HTTP transport failure (connect refused, TLS rejection, DNS, timeout,
    /// response-body read). Maps to `502` with `code = -1` per spec §10.5.
    /// PURA-220: shaped as a struct variant so the §7.0.2 `details` envelope
    /// can carry the typed `kind` prefix the operator banners render —
    /// dashboard / channels / clients / server-info paths previously
    /// surfaced reqwest's `Display` blob verbatim. The `Display` impl
    /// keeps the `transport error:` sentinel for §10.5 compat.
    #[error("transport error: {0}")]
    Transport(ClassifiedTransport),

    /// Response was reachable but not the expected `{body, status}` envelope.
    #[error("malformed WebQuery response: {0}")]
    InvalidResponse(String),

    /// `apiKey` ciphertext failed to decrypt. Construction-time only.
    #[error("failed to decrypt apiKey for connection #{config_id}: {source}")]
    Decrypt {
        config_id: i64,
        #[source]
        source: crate::crypto::AeadError,
    },
}

impl WebQueryError {
    /// HTTP status code for this error per §7.0.2. Used by the REST layer
    /// to translate uniformly.
    pub fn http_status(&self) -> StatusCode {
        match self {
            WebQueryError::Upstream { .. } => StatusCode::BAD_GATEWAY,
            WebQueryError::Transport(_) => StatusCode::BAD_GATEWAY,
            WebQueryError::InvalidResponse(_) => StatusCode::BAD_GATEWAY,
            WebQueryError::Decrypt { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// TS upstream code surfaced in the §7.0.2 body. Non-upstream errors
    /// report `-1` per spec §10.5.
    pub fn upstream_code(&self) -> i64 {
        match self {
            WebQueryError::Upstream { code, .. } => *code,
            _ => -1,
        }
    }

    /// Operator-friendly `details` string for the §7.0.2 body. Transport
    /// failures render as `"<kind>: <message>"` (PURA-220) so the operator
    /// banner picks up the class prefix without having to parse English.
    pub fn upstream_message(&self) -> String {
        match self {
            WebQueryError::Upstream { message, .. } => message.clone(),
            WebQueryError::Transport(ct) => ct.formatted(),
            other => other.to_string(),
        }
    }

    /// Typed transport-classifier kind, when this error is a transport
    /// failure. Returns `None` for envelope-level errors. The dashboard
    /// banner and `routes::control` audit log can branch on this without
    /// having to match the full `Transport(ClassifiedTransport)` shape.
    pub fn transport_kind(&self) -> Option<WebQueryTransportKind> {
        match self {
            WebQueryError::Transport(ct) => Some(ct.kind),
            _ => None,
        }
    }
}

impl WebQueryError {
    /// Builder for the `Transport` variant carrying [`other_transport`]
    /// shape — short-hand for the static-message call sites
    /// (`apiKey is not a valid HTTP header`, "No connection configured
    /// for server config ID X", builder failures).
    pub(crate) fn transport_other(message: impl Into<String>) -> Self {
        WebQueryError::Transport(transport_class::other_static(message))
    }
}

pub type WebQueryResult<T> = Result<T, WebQueryError>;

/// One WebQuery client per `server_connection` row (§10.1).
///
/// Single-socket invariant: enforced by `pool_max_idle_per_host(1)` plus an
/// async [`Mutex`] around the request-issuing path. Concurrent dashboard
/// loads queue rather than open a second socket, so the upstream's
/// ServerQuery clientlist sees exactly one `serveradmin*` slot per managed
/// server (verifiable via §10.9).
pub struct WebQueryClient {
    /// `server_connection.id` — debug only, never logged with credentials.
    config_id: i64,
    base_url: String,
    api_key: String,
    inner: Client,
    /// Serialises requests on a single permit so concurrent callers queue.
    request_lock: Mutex<()>,
}

impl std::fmt::Debug for WebQueryClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebQueryClient")
            .field("config_id", &self.config_id)
            .field("base_url", &self.base_url)
            // never expose api_key
            .finish_non_exhaustive()
    }
}

impl WebQueryClient {
    /// Build a client from the decrypted parameters. Most callers use
    /// [`WebQueryClient::from_connection`] which decrypts the apiKey via
    /// [`crate::crypto::unseal`] before delegating here.
    pub fn new(
        config_id: i64,
        host: &str,
        port: u16,
        use_https: bool,
        api_key: String,
        allow_self_signed: bool,
    ) -> WebQueryResult<Self> {
        let scheme = if use_https { "https" } else { "http" };
        let base_url = format!("{scheme}://{host}:{port}");

        let mut builder = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            // §10.1 single-socket invariant.
            .pool_max_idle_per_host(1)
            .http1_only()
            .pool_idle_timeout(Some(Duration::from_secs(90)));

        if allow_self_signed {
            // `TS_ALLOW_SELF_SIGNED` is a deliberate self-host escape hatch
            // (operator-controlled trust). Only honoured when the env flag
            // was true at boot — see [`crate::config::Config`].
            builder = builder.danger_accept_invalid_certs(true);
        }

        let inner = builder
            .build()
            .map_err(|e| WebQueryError::transport_other(e.to_string()))?;

        Ok(Self {
            config_id,
            base_url,
            api_key,
            inner,
            request_lock: Mutex::new(()),
        })
    }

    /// Build a client from a `ServerConnection` row. Decrypts `apiKey` via
    /// [`crate::crypto::unseal`] (legacy plaintext rows pass through per the
    /// crypto module's pass-through rule).
    pub fn from_connection(
        connection: &ServerConnection,
        allow_self_signed: bool,
    ) -> WebQueryResult<Self> {
        let api_key = crypto::unseal(&connection.apiKey).map_err(|e| WebQueryError::Decrypt {
            config_id: connection.id,
            source: e,
        })?;
        Self::new(
            connection.id,
            &connection.host,
            connection.webqueryPort.try_into().unwrap_or(10080),
            connection.useHttps,
            api_key,
            allow_self_signed,
        )
    }

    /// Returns the connection's database row id. Useful for log fields.
    pub fn config_id(&self) -> i64 {
        self.config_id
    }

    /// Issue a GET request and parse the spec §10.5 envelope as a list-shaped
    /// body. Use [`Self::get_one`] for inherently single-row commands; the
    /// TS6 wire wraps those in a one-element array (see [`OneOrSingleton`]).
    ///
    /// `path` is everything after the host:port — e.g. `/version` for an
    /// instance-scoped command, `/3/serverinfo` for a sid-scoped one.
    /// `params` are URL-encoded by reqwest; per §10.4 we do **not** apply
    /// ServerQuery escaping here.
    async fn get<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        params: &[(&str, &str)],
    ) -> WebQueryResult<T> {
        self.request(Method::GET, path, params).await
    }

    /// Singleton variant of [`Self::get`]. TS6's HTTP query interface wraps
    /// `body` in a one-element JSON array even for inherently single-row
    /// commands (`serverinfo`, `hostinfo`, `version`, `clientinfo`,
    /// `channelinfo`, `banadd`, …). This helper accepts either `body: {...}`
    /// (the legacy TS3 shape preserved for older fixtures) or `body: [{...}]`
    /// (the TS6-beta9 shape captured in the field) and yields one `T`.
    ///
    /// Note: the dispatch is done on `serde_json::Value` rather than via an
    /// `#[serde(untagged)]` wrapper. Some response models (`HostInfo`,
    /// `ConnectionInfo`) declare every field with `#[serde(default)]`, which
    /// causes the auto-derived `visit_seq` path to accept a JSON array as a
    /// positional struct and silently default every field. Going through
    /// `Value` makes the array-vs-object decision before model decoding.
    async fn get_one<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        params: &[(&str, &str)],
    ) -> WebQueryResult<T> {
        let body: serde_json::Value = self.request(Method::GET, path, params).await?;
        let one = unwrap_singleton_body(body)?;
        serde_json::from_value(one)
            .map_err(|e| WebQueryError::InvalidResponse(format!("singleton body decode: {e}")))
    }

    async fn request<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        path: &str,
        params: &[(&str, &str)],
    ) -> WebQueryResult<T> {
        let span = tracing::info_span!(
            "webquery.request",
            config_id = self.config_id,
            method = %method,
            path,
        );
        async move {
            // §10.1 — serialise to the single keep-alive socket.
            let _permit = self.request_lock.lock().await;

            let url = format!("{}{}", self.base_url, path);

            let mut headers = HeaderMap::new();
            let key = HeaderValue::from_str(&self.api_key)
                .map_err(|_| WebQueryError::transport_other("apiKey is not a valid HTTP header"))?;
            headers.insert(API_KEY_HEADER, key);

            let started = Instant::now();
            let send_result = self
                .inner
                .request(method.clone(), &url)
                .headers(headers)
                .query(params)
                .send()
                .await;

            let response = match send_result {
                Ok(resp) => resp,
                Err(e) => {
                    let latency_ms = started.elapsed().as_millis() as u64;
                    // PURA-220: classify so the §7.0.2 `details` and the
                    // dashboard banner pick up "Connection refused …" /
                    // "DNS lookup failed …" instead of reqwest's `Display`
                    // blob. The same `url` we just dialed feeds the
                    // operator-facing message verbatim.
                    let classified = transport_class::classify_reqwest_error(&e, &url);
                    tracing::warn!(
                        latency_ms,
                        outcome = "transport",
                        kind = classified.kind.as_str(),
                        error = %e,
                    );
                    return Err(WebQueryError::Transport(classified));
                }
            };

            // §10.5 — body is the canonical signal even on non-2xx; only fall
            // back to status-only error messaging when the body fails to parse.
            let bytes = match response.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    let latency_ms = started.elapsed().as_millis() as u64;
                    let classified = transport_class::classify_response_body_error(&e, &url);
                    tracing::warn!(
                        latency_ms,
                        outcome = "transport",
                        kind = classified.kind.as_str(),
                        error = %e,
                    );
                    return Err(WebQueryError::Transport(classified));
                }
            };

            let latency_ms = started.elapsed().as_millis() as u64;

            let envelope: Envelope = serde_json::from_slice(&bytes).map_err(|e| {
                tracing::warn!(latency_ms, outcome = "invalid_response", error = %e);
                WebQueryError::InvalidResponse(format!("envelope parse failed: {e}"))
            })?;

            match envelope.into_body::<T>() {
                Ok(body) => {
                    tracing::debug!(latency_ms, outcome = "ok");
                    Ok(body)
                }
                Err(WebQueryError::Upstream { code, message }) => {
                    tracing::info!(latency_ms, outcome = "upstream_err", code, %message);
                    Err(WebQueryError::Upstream { code, message })
                }
                Err(other) => {
                    tracing::warn!(latency_ms, outcome = "invalid_response", error = %other);
                    Err(other)
                }
            }
        }
        .instrument(span)
        .await
    }

    /// Run a list-shaped read and coerce TS upstream code `1281`
    /// (`database_empty_result`) to an empty `Vec` per spec §10.6. Used by
    /// `clientpermlist` / `channelclientpermlist` / `ftgetfilelist`-style
    /// commands where "no rows" arrives as an upstream error rather than a
    /// `body: []`.
    async fn list_or_empty<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        params: &[(&str, &str)],
    ) -> WebQueryResult<Vec<T>> {
        match self.get::<Vec<T>>(path, params).await {
            Ok(v) => Ok(v),
            Err(WebQueryError::Upstream { code, .. }) if code == DATABASE_EMPTY_RESULT => {
                Ok(Vec::new())
            }
            Err(e) => Err(e),
        }
    }

    /// List read that tolerates the two TS6 "no rows" shapes: upstream
    /// code `1281` (`database_empty_result`) **and** an `ok` envelope with
    /// no `body` at all (`servergroupclientlist` on an empty group). Both
    /// collapse to `[]` per spec §7.9 / §7.16 (PURA-373). Decoding through
    /// `Option<Vec<T>>` lets the absent-body case land as `None`.
    async fn list_lenient<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        params: &[(&str, &str)],
    ) -> WebQueryResult<Vec<T>> {
        match self.get::<Option<Vec<T>>>(path, params).await {
            Ok(v) => Ok(v.unwrap_or_default()),
            Err(WebQueryError::Upstream { code, .. }) if code == DATABASE_EMPTY_RESULT => {
                Ok(Vec::new())
            }
            Err(e) => Err(e),
        }
    }

    /// `version` (instance scope). Phase 1 health probe per §10.7.
    pub async fn version(&self) -> WebQueryResult<VersionInfo> {
        self.get_one::<VersionInfo>("/version", &[]).await
    }

    /// Returns true if [`Self::version`] succeeds. Suitable for the cheap
    /// dashboard health gate.
    pub async fn health(&self) -> bool {
        self.version().await.is_ok()
    }

    /// `serverlist` (instance scope) — drives `vs:sid` enumeration in the
    /// virtual-server selector.
    pub async fn serverlist(&self) -> WebQueryResult<Vec<VirtualServerEntry>> {
        self.get::<Vec<VirtualServerEntry>>("/serverlist", &[])
            .await
    }

    /// `serverinfo` (sid scope).
    pub async fn serverinfo(&self, sid: i64) -> WebQueryResult<ServerInfo> {
        self.get_one::<ServerInfo>(&format!("/{sid}/serverinfo"), &[])
            .await
    }

    /// `channellist` (sid scope).
    pub async fn channellist(&self, sid: i64) -> WebQueryResult<Vec<ChannelEntry>> {
        self.get::<Vec<ChannelEntry>>(&format!("/{sid}/channellist"), &[])
            .await
    }

    /// `clientlist` (sid scope).
    pub async fn clientlist(&self, sid: i64) -> WebQueryResult<Vec<ClientEntry>> {
        self.get::<Vec<ClientEntry>>(&format!("/{sid}/clientlist"), &[])
            .await
    }

    /// `serverrequestconnectioninfo` (sid scope).
    pub async fn server_connection_info(&self, sid: i64) -> WebQueryResult<ConnectionInfo> {
        self.get_one::<ConnectionInfo>(&format!("/{sid}/serverrequestconnectioninfo"), &[])
            .await
    }

    // =====================================================================
    // Phase 2 (PURA-68) — full ServerQuery command surface
    // =====================================================================

    /// `clientlist` with the §7.8 flag set. Pass an empty slice for the
    /// minimal `clid`/`cid`/`client_database_id`/`client_type`/`client_nickname`
    /// projection. Standard flags: `-uid -away -voice -times -groups -info
    /// -country`; `-ip` is admin-only and the route layer is responsible for
    /// gating it.
    pub async fn clientlist_with_flags(
        &self,
        sid: i64,
        flags: &[&str],
    ) -> WebQueryResult<Vec<ClientEntry>> {
        let path = format!("/{sid}/clientlist{}", flag_suffix(flags));
        self.get::<Vec<ClientEntry>>(&path, &[]).await
    }

    /// `clientinfo` (sid scope).
    pub async fn clientinfo(&self, sid: i64, clid: i64) -> WebQueryResult<ClientInfo> {
        let clid_s = clid.to_string();
        self.get_one::<ClientInfo>(&format!("/{sid}/clientinfo"), &[("clid", clid_s.as_str())])
            .await
    }

    /// `clientdblist` (sid scope) — paginated. Defaults per §7.8: `start=0`,
    /// `duration=100`. The route layer enforces operator-supplied bounds.
    pub async fn clientdblist(
        &self,
        sid: i64,
        start: i64,
        duration: i64,
    ) -> WebQueryResult<Vec<ClientDbEntry>> {
        let start_s = start.to_string();
        let dur_s = duration.to_string();
        self.get::<Vec<ClientDbEntry>>(
            &format!("/{sid}/clientdblist"),
            &[("start", start_s.as_str()), ("duration", dur_s.as_str())],
        )
        .await
    }

    /// `clientdbinfo` (sid scope).
    pub async fn clientdbinfo(&self, sid: i64, cldbid: i64) -> WebQueryResult<ClientDbEntry> {
        let cldbid_s = cldbid.to_string();
        self.get_one::<ClientDbEntry>(
            &format!("/{sid}/clientdbinfo"),
            &[("cldbid", cldbid_s.as_str())],
        )
        .await
    }

    /// `channellist` with optional flags. Pass an empty slice for the minimal
    /// projection (`cid`/`channel_name`/`pid`/`channel_order`). §7.7 mandates
    /// `-topic -flags -voice -limits -icon -secondsempty` at the REST layer.
    pub async fn channellist_with_flags(
        &self,
        sid: i64,
        flags: &[&str],
    ) -> WebQueryResult<Vec<ChannelEntry>> {
        let path = format!("/{sid}/channellist{}", flag_suffix(flags));
        self.get::<Vec<ChannelEntry>>(&path, &[]).await
    }

    /// `channelinfo` (sid scope).
    pub async fn channelinfo(&self, sid: i64, cid: i64) -> WebQueryResult<ChannelInfo> {
        let cid_s = cid.to_string();
        self.get_one::<ChannelInfo>(&format!("/{sid}/channelinfo"), &[("cid", cid_s.as_str())])
            .await
    }

    /// `channelclientlist` — clients in a specific channel.
    pub async fn channelclientlist(&self, sid: i64, cid: i64) -> WebQueryResult<Vec<ClientEntry>> {
        let cid_s = cid.to_string();
        self.get::<Vec<ClientEntry>>(
            &format!("/{sid}/channelclientlist"),
            &[("cid", cid_s.as_str())],
        )
        .await
    }

    /// `hostinfo` (instance scope).
    pub async fn hostinfo(&self) -> WebQueryResult<HostInfo> {
        self.get_one::<HostInfo>("/hostinfo", &[]).await
    }

    /// `logview` (sid scope) — paginated log retrieval. Defaults follow
    /// §7.17: `lines=100`, `reverse=1`, `instance=0`. `begin_pos` is omitted
    /// when `None` (initial fetch); pass the previous response's `last_pos`
    /// to page forward.
    pub async fn logview(
        &self,
        sid: i64,
        lines: u32,
        reverse: bool,
        instance: bool,
        begin_pos: Option<i64>,
    ) -> WebQueryResult<Vec<LogEntry>> {
        let lines_s = lines.to_string();
        let reverse_s = if reverse { "1" } else { "0" };
        let instance_s = if instance { "1" } else { "0" };
        let mut params: Vec<(&str, &str)> = vec![
            ("lines", lines_s.as_str()),
            ("reverse", reverse_s),
            ("instance", instance_s),
        ];
        let begin_s;
        if let Some(pos) = begin_pos {
            begin_s = pos.to_string();
            params.push(("begin_pos", begin_s.as_str()));
        }
        self.get::<Vec<LogEntry>>(&format!("/{sid}/logview"), &params)
            .await
    }

    /// `clientkick` (sid scope) — moderator action. `reasonid` defaults to
    /// `5` (server kick) per §7.8 / §14.1.
    pub async fn clientkick(
        &self,
        sid: i64,
        clid: i64,
        reasonid: i64,
        reasonmsg: Option<&str>,
    ) -> WebQueryResult<()> {
        let clid_s = clid.to_string();
        let reasonid_s = reasonid.to_string();
        let mut params: Vec<(&str, &str)> =
            vec![("clid", clid_s.as_str()), ("reasonid", reasonid_s.as_str())];
        if let Some(msg) = reasonmsg {
            params.push(("reasonmsg", msg));
        }
        self.get::<UnitBody>(&format!("/{sid}/clientkick"), &params)
            .await?;
        Ok(())
    }

    /// `clientpoke` (sid scope) — fire a popup at the targeted client.
    pub async fn clientpoke(&self, sid: i64, clid: i64, msg: &str) -> WebQueryResult<()> {
        let clid_s = clid.to_string();
        self.get::<UnitBody>(
            &format!("/{sid}/clientpoke"),
            &[("clid", clid_s.as_str()), ("msg", msg)],
        )
        .await?;
        Ok(())
    }

    /// `clientmove` (sid scope) — force-move client to channel `cid`. `cpw`
    /// is the optional channel password.
    pub async fn clientmove(
        &self,
        sid: i64,
        clid: i64,
        cid: i64,
        cpw: Option<&str>,
    ) -> WebQueryResult<()> {
        let clid_s = clid.to_string();
        let cid_s = cid.to_string();
        let mut params: Vec<(&str, &str)> =
            vec![("clid", clid_s.as_str()), ("cid", cid_s.as_str())];
        if let Some(pw) = cpw {
            params.push(("cpw", pw));
        }
        self.get::<UnitBody>(&format!("/{sid}/clientmove"), &params)
            .await?;
        Ok(())
    }

    /// `clientedit` (sid scope) — flexible primitive for property changes
    /// (e.g. `CLIENT_DESCRIPTION`, `CLIENT_IS_TALKER`). Used by
    /// [`Self::client_set_talker`].
    pub async fn clientedit_raw(
        &self,
        sid: i64,
        clid: i64,
        props: &[(&str, &str)],
    ) -> WebQueryResult<()> {
        let clid_s = clid.to_string();
        let mut params: Vec<(&str, &str)> = Vec::with_capacity(props.len() + 1);
        params.push(("clid", clid_s.as_str()));
        params.extend_from_slice(props);
        self.get::<UnitBody>(&format!("/{sid}/clientedit"), &params)
            .await?;
        Ok(())
    }

    /// Talker-flag helper — the genuine TS6 6.0 server-side
    /// voice-suppression primitive (PURA-292). `can_talk == false`
    /// revokes talk permission ("mute"); `true` restores it ("unmute").
    ///
    /// `client_input_muted` / `client_output_muted` are **client-self**
    /// state — TS6 6.0.0-beta rejects them on `clientedit` for any other
    /// client with `1538 invalid parameter`, so [`Self::client_set_muted`]
    /// cannot silence a third party on a live host. `client_is_talker`
    /// *is* server-editable and is honoured in moderated channels
    /// (`channel_needed_talk_power > 0`).
    ///
    /// TS6 accepts `client_is_talker=0` in any channel, but rejects `=1`
    /// with `1538` when the target is not in a moderated channel — callers
    /// that "unmute" must tolerate that code (the client can already speak
    /// there). `&[(&str, &str)]` keeps this on the audited `clientedit`
    /// path shared with [`Self::clientedit_raw`].
    pub async fn client_set_talker(
        &self,
        sid: i64,
        clid: i64,
        can_talk: bool,
    ) -> WebQueryResult<()> {
        self.clientedit_raw(sid, clid, &[("client_is_talker", bool_to_int(can_talk))])
            .await
    }

    /// `sendtextmessage` (sid scope) — deliver a text message to a client
    /// (`targetmode=1`, `target=clid`), a channel (`targetmode=2`,
    /// `target=cid`), or the whole virtual server (`targetmode=3`,
    /// `target=sid`). Powers the `welcome-on-join` flow example
    /// (`docs/flows/http-api.md` §3.1).
    pub async fn sendtextmessage(
        &self,
        sid: i64,
        targetmode: i64,
        target: i64,
        msg: &str,
    ) -> WebQueryResult<()> {
        let targetmode_s = targetmode.to_string();
        let target_s = target.to_string();
        self.get::<UnitBody>(
            &format!("/{sid}/sendtextmessage"),
            &[
                ("targetmode", targetmode_s.as_str()),
                ("target", target_s.as_str()),
                ("msg", msg),
            ],
        )
        .await?;
        Ok(())
    }

    /// `servergroupaddclient` (sid scope) — add the client database id
    /// `cldbid` to server group `sgid`. Powers the auto-group-assign flow
    /// example (`docs/flows/architecture.md` §4).
    pub async fn servergroupaddclient(
        &self,
        sid: i64,
        sgid: i64,
        cldbid: i64,
    ) -> WebQueryResult<()> {
        let sgid_s = sgid.to_string();
        let cldbid_s = cldbid.to_string();
        self.get::<UnitBody>(
            &format!("/{sid}/servergroupaddclient"),
            &[("sgid", sgid_s.as_str()), ("cldbid", cldbid_s.as_str())],
        )
        .await?;
        Ok(())
    }

    /// `banlist` (sid scope).
    pub async fn banlist(&self, sid: i64) -> WebQueryResult<Vec<BanEntry>> {
        // Empty banlist surfaces as upstream 1281 on some upstreams.
        self.list_or_empty::<BanEntry>(&format!("/{sid}/banlist"), &[])
            .await
    }

    /// `banadd` (sid scope) — returns the new ban id. Per §7.12 the route
    /// forwards `ip` / `uid` / `mytsid` / `name` / `banreason` / `time`;
    /// `time = 0` is permanent.
    pub async fn banadd(&self, sid: i64, params: &BanAddParams<'_>) -> WebQueryResult<i64> {
        let mut q: Vec<(&str, &str)> = Vec::with_capacity(6);
        if let Some(v) = params.ip {
            q.push(("ip", v));
        }
        if let Some(v) = params.uid {
            q.push(("uid", v));
        }
        if let Some(v) = params.mytsid {
            q.push(("mytsid", v));
        }
        if let Some(v) = params.name {
            q.push(("name", v));
        }
        if let Some(v) = params.banreason {
            q.push(("banreason", v));
        }
        let time_s;
        if let Some(t) = params.time {
            time_s = t.to_string();
            q.push(("time", time_s.as_str()));
        }
        let resp: BanAddResponse = self.get_one(&format!("/{sid}/banadd"), &q).await?;
        Ok(resp.banid)
    }

    /// `bandel` (sid scope) — drop a single ban by id.
    pub async fn bandel(&self, sid: i64, banid: i64) -> WebQueryResult<()> {
        let banid_s = banid.to_string();
        self.get::<UnitBody>(&format!("/{sid}/bandel"), &[("banid", banid_s.as_str())])
            .await?;
        Ok(())
    }

    /// `bandelall` (sid scope) — drop every ban on this virtual server.
    pub async fn bandelall(&self, sid: i64) -> WebQueryResult<()> {
        self.get::<UnitBody>(&format!("/{sid}/bandelall"), &[])
            .await?;
        Ok(())
    }

    /// `channelclientpermlist` — list a client's per-channel permission rows.
    /// TS upstream code 1281 (`database_empty_result`) is mapped to `[]`
    /// per §10.6 to match the route-layer contract.
    pub async fn channelclientpermlist(
        &self,
        sid: i64,
        cid: i64,
        cldbid: i64,
    ) -> WebQueryResult<Vec<ChannelClientPerm>> {
        let cid_s = cid.to_string();
        let cldbid_s = cldbid.to_string();
        // `-permsid` toggles the symbolic name in the response (matches the
        // §7.7 channelpermlist projection). Same query-string flag rule as
        // `clientlist`/`channellist` — TS6 rejects path-suffix concatenation.
        self.list_or_empty::<ChannelClientPerm>(
            &format!("/{sid}/channelclientpermlist{}", flag_suffix(&["permsid"])),
            &[("cid", cid_s.as_str()), ("cldbid", cldbid_s.as_str())],
        )
        .await
    }

    /// `complainlist` (sid scope) — the TS6 complaint queue. `tcldbid`
    /// filters to one target subject (spec §7.15); `None` returns the
    /// whole queue. Empty complaint lists surface as upstream `1281`
    /// (`database_empty_result`); collapse that to `[]` via
    /// [`Self::list_or_empty`], same as `banlist` (`9.0-spike`).
    pub async fn complainlist(
        &self,
        sid: i64,
        tcldbid: Option<i64>,
    ) -> WebQueryResult<Vec<ComplaintEntry>> {
        let tcldbid_s;
        let params: Vec<(&str, &str)> = if let Some(t) = tcldbid {
            tcldbid_s = t.to_string();
            vec![("tcldbid", tcldbid_s.as_str())]
        } else {
            Vec::new()
        };
        self.list_or_empty::<ComplaintEntry>(&format!("/{sid}/complainlist"), &params)
            .await
    }

    /// `complaindel` (sid scope) — dismiss one complaint, identified by
    /// its `(tcldbid, fcldbid)` pair. Per the `9.0-spike` findings TS6
    /// returns `512` ("invalid clientID") for both an invalid id and a
    /// non-existent complaint — the two are indistinguishable; the route
    /// layer maps `512` → `404`.
    pub async fn complaindel(&self, sid: i64, tcldbid: i64, fcldbid: i64) -> WebQueryResult<()> {
        let tcldbid_s = tcldbid.to_string();
        let fcldbid_s = fcldbid.to_string();
        self.get::<UnitBody>(
            &format!("/{sid}/complaindel"),
            &[
                ("tcldbid", tcldbid_s.as_str()),
                ("fcldbid", fcldbid_s.as_str()),
            ],
        )
        .await?;
        Ok(())
    }

    /// `complaindelall` (sid scope) — dismiss **every** complaint about
    /// one target. Per-target: `tcldbid` is required (`9.0-spike`), this
    /// is not a vserver-wide purge. Idempotent — a dismiss-all on an
    /// already-clean target succeeds with code 0.
    pub async fn complaindelall(&self, sid: i64, tcldbid: i64) -> WebQueryResult<()> {
        let tcldbid_s = tcldbid.to_string();
        self.get::<UnitBody>(
            &format!("/{sid}/complaindelall"),
            &[("tcldbid", tcldbid_s.as_str())],
        )
        .await?;
        Ok(())
    }

    // =====================================================================
    // PURA-373 — server-group command surface (spec §7.9)
    // =====================================================================

    /// `servergrouplist` (sid scope).
    pub async fn servergrouplist(&self, sid: i64) -> WebQueryResult<Vec<ServerGroupEntry>> {
        self.get::<Vec<ServerGroupEntry>>(&format!("/{sid}/servergrouplist"), &[])
            .await
    }

    /// `servergroupadd` (sid scope) — create a server group. `group_type`
    /// is left to the upstream default when `None` (a regular group).
    pub async fn servergroupadd(
        &self,
        sid: i64,
        name: &str,
        group_type: Option<i64>,
    ) -> WebQueryResult<i64> {
        let type_s;
        let mut q: Vec<(&str, &str)> = vec![("name", name)];
        if let Some(t) = group_type {
            type_s = t.to_string();
            q.push(("type", type_s.as_str()));
        }
        let resp: ServerGroupIdResponse =
            self.get_one(&format!("/{sid}/servergroupadd"), &q).await?;
        Ok(resp.sgid)
    }

    /// `servergrouprename` (sid scope).
    pub async fn servergrouprename(&self, sid: i64, sgid: i64, name: &str) -> WebQueryResult<()> {
        let sgid_s = sgid.to_string();
        self.get::<UnitBody>(
            &format!("/{sid}/servergrouprename"),
            &[("sgid", sgid_s.as_str()), ("name", name)],
        )
        .await?;
        Ok(())
    }

    /// `servergroupdel` (sid scope). Always passes `force=1` per spec §7.9
    /// so a non-empty group is removed in a single call.
    pub async fn servergroupdel(&self, sid: i64, sgid: i64) -> WebQueryResult<()> {
        let sgid_s = sgid.to_string();
        self.get::<UnitBody>(
            &format!("/{sid}/servergroupdel"),
            &[("sgid", sgid_s.as_str()), ("force", "1")],
        )
        .await?;
        Ok(())
    }

    /// `servergroupcopy` (sid scope) — copy `ssgid` into a brand-new group
    /// (`tsgid=0`) named `name`. Returns the new `sgid`.
    pub async fn servergroupcopy(
        &self,
        sid: i64,
        ssgid: i64,
        name: &str,
        group_type: i64,
    ) -> WebQueryResult<i64> {
        let ssgid_s = ssgid.to_string();
        let type_s = group_type.to_string();
        let resp: ServerGroupIdResponse = self
            .get_one(
                &format!("/{sid}/servergroupcopy"),
                &[
                    ("ssgid", ssgid_s.as_str()),
                    ("tsgid", "0"),
                    ("name", name),
                    ("type", type_s.as_str()),
                ],
            )
            .await?;
        Ok(resp.sgid)
    }

    /// `servergroupclientlist -names` (sid scope) — group members. An empty
    /// group returns a body-less `ok` envelope; collapsed to `[]`.
    pub async fn servergroupclientlist(
        &self,
        sid: i64,
        sgid: i64,
    ) -> WebQueryResult<Vec<ServerGroupClient>> {
        let sgid_s = sgid.to_string();
        self.list_lenient::<ServerGroupClient>(
            &format!("/{sid}/servergroupclientlist{}", flag_suffix(&["names"])),
            &[("sgid", sgid_s.as_str())],
        )
        .await
    }

    /// `servergroupdelclient` (sid scope) — remove a member from a group.
    pub async fn servergroupdelclient(
        &self,
        sid: i64,
        sgid: i64,
        cldbid: i64,
    ) -> WebQueryResult<()> {
        let sgid_s = sgid.to_string();
        let cldbid_s = cldbid.to_string();
        self.get::<UnitBody>(
            &format!("/{sid}/servergroupdelclient"),
            &[("sgid", sgid_s.as_str()), ("cldbid", cldbid_s.as_str())],
        )
        .await?;
        Ok(())
    }

    /// `servergrouppermlist -permsid` (sid scope) — per-group permission
    /// rows keyed by the stable string `permsid`.
    pub async fn servergrouppermlist(
        &self,
        sid: i64,
        sgid: i64,
    ) -> WebQueryResult<Vec<GroupPermEntry>> {
        let sgid_s = sgid.to_string();
        self.list_lenient::<GroupPermEntry>(
            &format!("/{sid}/servergrouppermlist{}", flag_suffix(&["permsid"])),
            &[("sgid", sgid_s.as_str())],
        )
        .await
    }

    /// `servergroupaddperm` (sid scope) — upsert one permission on a group.
    /// `permsid` is accepted directly — no numeric `permid` bridge is
    /// needed for group writes (PURA-370 §2).
    pub async fn servergroupaddperm(
        &self,
        sid: i64,
        sgid: i64,
        perm: &GroupPermWrite<'_>,
    ) -> WebQueryResult<()> {
        let sgid_s = sgid.to_string();
        let value_s = perm.permvalue.to_string();
        self.get::<UnitBody>(
            &format!("/{sid}/servergroupaddperm"),
            &[
                ("sgid", sgid_s.as_str()),
                ("permsid", perm.permsid),
                ("permvalue", value_s.as_str()),
                ("permnegated", bool_to_int(perm.permnegated)),
                ("permskip", bool_to_int(perm.permskip)),
            ],
        )
        .await?;
        Ok(())
    }

    /// `servergroupdelperm` (sid scope) — drop one permission from a group.
    pub async fn servergroupdelperm(
        &self,
        sid: i64,
        sgid: i64,
        permsid: &str,
    ) -> WebQueryResult<()> {
        let sgid_s = sgid.to_string();
        self.get::<UnitBody>(
            &format!("/{sid}/servergroupdelperm"),
            &[("sgid", sgid_s.as_str()), ("permsid", permsid)],
        )
        .await?;
        Ok(())
    }

    // =====================================================================
    // PURA-373 — channel-group command surface (spec §7.10)
    // =====================================================================

    /// `channelgrouplist` (sid scope).
    pub async fn channelgrouplist(&self, sid: i64) -> WebQueryResult<Vec<ChannelGroupEntry>> {
        self.get::<Vec<ChannelGroupEntry>>(&format!("/{sid}/channelgrouplist"), &[])
            .await
    }

    /// `channelgroupadd` (sid scope).
    pub async fn channelgroupadd(
        &self,
        sid: i64,
        name: &str,
        group_type: Option<i64>,
    ) -> WebQueryResult<i64> {
        let type_s;
        let mut q: Vec<(&str, &str)> = vec![("name", name)];
        if let Some(t) = group_type {
            type_s = t.to_string();
            q.push(("type", type_s.as_str()));
        }
        let resp: ChannelGroupIdResponse =
            self.get_one(&format!("/{sid}/channelgroupadd"), &q).await?;
        Ok(resp.cgid)
    }

    /// `channelgrouprename` (sid scope).
    pub async fn channelgrouprename(&self, sid: i64, cgid: i64, name: &str) -> WebQueryResult<()> {
        let cgid_s = cgid.to_string();
        self.get::<UnitBody>(
            &format!("/{sid}/channelgrouprename"),
            &[("cgid", cgid_s.as_str()), ("name", name)],
        )
        .await?;
        Ok(())
    }

    /// `channelgroupdel` (sid scope) — always passes `force=1`.
    pub async fn channelgroupdel(&self, sid: i64, cgid: i64) -> WebQueryResult<()> {
        let cgid_s = cgid.to_string();
        self.get::<UnitBody>(
            &format!("/{sid}/channelgroupdel"),
            &[("cgid", cgid_s.as_str()), ("force", "1")],
        )
        .await?;
        Ok(())
    }

    /// `channelgroupclientlist` (sid scope) — the `(cid, cldbid, cgid)`
    /// assignments for one channel group.
    pub async fn channelgroupclientlist(
        &self,
        sid: i64,
        cgid: i64,
    ) -> WebQueryResult<Vec<ChannelGroupClient>> {
        let cgid_s = cgid.to_string();
        self.list_lenient::<ChannelGroupClient>(
            &format!("/{sid}/channelgroupclientlist"),
            &[("cgid", cgid_s.as_str())],
        )
        .await
    }

    /// `setclientchannelgroup` (sid scope) — assign `cldbid` to channel
    /// group `cgid` within channel `cid`.
    pub async fn setclientchannelgroup(
        &self,
        sid: i64,
        cgid: i64,
        cid: i64,
        cldbid: i64,
    ) -> WebQueryResult<()> {
        let cgid_s = cgid.to_string();
        let cid_s = cid.to_string();
        let cldbid_s = cldbid.to_string();
        self.get::<UnitBody>(
            &format!("/{sid}/setclientchannelgroup"),
            &[
                ("cgid", cgid_s.as_str()),
                ("cid", cid_s.as_str()),
                ("cldbid", cldbid_s.as_str()),
            ],
        )
        .await?;
        Ok(())
    }

    /// `channelgrouppermlist -permsid` (sid scope).
    pub async fn channelgrouppermlist(
        &self,
        sid: i64,
        cgid: i64,
    ) -> WebQueryResult<Vec<GroupPermEntry>> {
        let cgid_s = cgid.to_string();
        self.list_lenient::<GroupPermEntry>(
            &format!("/{sid}/channelgrouppermlist{}", flag_suffix(&["permsid"])),
            &[("cgid", cgid_s.as_str())],
        )
        .await
    }

    /// `channelgroupaddperm` (sid scope). TS6 channel-group permissions
    /// carry only a value — `permnegated` / `permskip` are server-group /
    /// client concepts and are not part of this command.
    pub async fn channelgroupaddperm(
        &self,
        sid: i64,
        cgid: i64,
        permsid: &str,
        permvalue: i64,
    ) -> WebQueryResult<()> {
        let cgid_s = cgid.to_string();
        let value_s = permvalue.to_string();
        self.get::<UnitBody>(
            &format!("/{sid}/channelgroupaddperm"),
            &[
                ("cgid", cgid_s.as_str()),
                ("permsid", permsid),
                ("permvalue", value_s.as_str()),
            ],
        )
        .await?;
        Ok(())
    }

    /// `channelgroupdelperm` (sid scope).
    pub async fn channelgroupdelperm(
        &self,
        sid: i64,
        cgid: i64,
        permsid: &str,
    ) -> WebQueryResult<()> {
        let cgid_s = cgid.to_string();
        self.get::<UnitBody>(
            &format!("/{sid}/channelgroupdelperm"),
            &[("cgid", cgid_s.as_str()), ("permsid", permsid)],
        )
        .await?;
        Ok(())
    }

    // =====================================================================
    // PURA-373 — permission catalog command surface (spec §7.11, read-only)
    // =====================================================================

    /// `permissionlist` — the full permission catalog (spec §7.11). Scoped
    /// under `sid` so the WebQuery session has a server selected; the
    /// command itself is instance-global.
    pub async fn permissionlist(&self, sid: i64) -> WebQueryResult<Vec<PermissionEntry>> {
        self.get::<Vec<PermissionEntry>>(&format!("/{sid}/permissionlist"), &[])
            .await
    }

    /// `permfind` — locate every assignment of a permission, selected by
    /// numeric `permid` or string `permsid`.
    pub async fn permfind(
        &self,
        sid: i64,
        selector: PermSelector<'_>,
    ) -> WebQueryResult<Vec<PermFindEntry>> {
        let permid_s;
        let params: Vec<(&str, &str)> = match selector {
            PermSelector::Id(id) => {
                permid_s = id.to_string();
                vec![("permid", permid_s.as_str())]
            }
            PermSelector::Sid(s) => vec![("permsid", s)],
        };
        self.list_lenient::<PermFindEntry>(&format!("/{sid}/permfind"), &params)
            .await
    }

    /// `permidgetbyname` — bridge a `permsid` string to its numeric
    /// `permid` (spec §7.8 client-perm writes).
    pub async fn permidgetbyname(&self, sid: i64, permsid: &str) -> WebQueryResult<i64> {
        let resp: PermIdEntry = self
            .get_one(&format!("/{sid}/permidgetbyname"), &[("permsid", permsid)])
            .await?;
        Ok(resp.permid)
    }

    /// `permoverview` — every permission in effect for a client, each row
    /// tagged with its origin (`t` / `id1`). `cid` and `permid` default to
    /// `0` per spec §7.11 (whole-catalog overview for the client).
    pub async fn permoverview(
        &self,
        sid: i64,
        cldbid: i64,
        cid: i64,
        permid: i64,
    ) -> WebQueryResult<Vec<PermOverviewEntry>> {
        let cldbid_s = cldbid.to_string();
        let cid_s = cid.to_string();
        let permid_s = permid.to_string();
        self.list_lenient::<PermOverviewEntry>(
            &format!("/{sid}/permoverview"),
            &[
                ("cldbid", cldbid_s.as_str()),
                ("cid", cid_s.as_str()),
                ("permid", permid_s.as_str()),
            ],
        )
        .await
    }

    // =====================================================================
    // PURA-373 — token (privilege key) command surface (spec §7.13)
    // =====================================================================

    /// `privilegekeylist` (sid scope).
    pub async fn privilegekeylist(&self, sid: i64) -> WebQueryResult<Vec<PrivilegeKeyEntry>> {
        self.list_lenient::<PrivilegeKeyEntry>(&format!("/{sid}/privilegekeylist"), &[])
            .await
    }

    /// `privilegekeyadd` (sid scope) — mint a privilege key. `token_type`
    /// is `0` for a server-group key (`id1 = sgid`, `id2 = 0`) or `1` for
    /// a channel-group key (`id1 = cgid`, `id2 = cid`). Returns the key.
    pub async fn privilegekeyadd(
        &self,
        sid: i64,
        params: &PrivilegeKeyAddParams<'_>,
    ) -> WebQueryResult<String> {
        let type_s = params.token_type.to_string();
        let id1_s = params.token_id1.to_string();
        let id2_s = params.token_id2.to_string();
        let mut q: Vec<(&str, &str)> = vec![
            ("tokentype", type_s.as_str()),
            ("tokenid1", id1_s.as_str()),
            ("tokenid2", id2_s.as_str()),
        ];
        if let Some(d) = params.description {
            q.push(("tokendescription", d));
        }
        if let Some(c) = params.customset {
            q.push(("tokencustomset", c));
        }
        let resp: PrivilegeKeyAddResponse =
            self.get_one(&format!("/{sid}/privilegekeyadd"), &q).await?;
        Ok(resp.token)
    }

    /// `privilegekeydelete` (sid scope).
    pub async fn privilegekeydelete(&self, sid: i64, token: &str) -> WebQueryResult<()> {
        self.get::<UnitBody>(&format!("/{sid}/privilegekeydelete"), &[("token", token)])
            .await?;
        Ok(())
    }

    // =====================================================================
    // PURA-373 — offline-message command surface (spec §7.16)
    // =====================================================================

    /// `messagelist` (sid scope) — the offline-message inbox. An empty
    /// inbox surfaces as upstream code 1281; collapsed to `[]`.
    pub async fn messagelist(&self, sid: i64) -> WebQueryResult<Vec<MessageEntry>> {
        self.list_lenient::<MessageEntry>(&format!("/{sid}/messagelist"), &[])
            .await
    }

    /// `messageget` (sid scope) — one message including its body.
    pub async fn messageget(&self, sid: i64, msgid: i64) -> WebQueryResult<MessageDetail> {
        let msgid_s = msgid.to_string();
        self.get_one::<MessageDetail>(
            &format!("/{sid}/messageget"),
            &[("msgid", msgid_s.as_str())],
        )
        .await
    }

    /// `messageadd` (sid scope) — leave an offline message for `cluid`.
    pub async fn messageadd(
        &self,
        sid: i64,
        cluid: &str,
        subject: &str,
        message: &str,
    ) -> WebQueryResult<()> {
        self.get::<UnitBody>(
            &format!("/{sid}/messageadd"),
            &[("cluid", cluid), ("subject", subject), ("message", message)],
        )
        .await?;
        Ok(())
    }

    /// `messagedel` (sid scope).
    pub async fn messagedel(&self, sid: i64, msgid: i64) -> WebQueryResult<()> {
        let msgid_s = msgid.to_string();
        self.get::<UnitBody>(
            &format!("/{sid}/messagedel"),
            &[("msgid", msgid_s.as_str())],
        )
        .await?;
        Ok(())
    }
}

/// Sentinel for write commands that only carry a `status` envelope. TS
/// returns `body: {}`, `body: []`, or `body: null` on these; we accept any
/// of them because [`Envelope::into_body`] forwards `null` as
/// [`serde_json::Value::Null`] and `Object(Value)` here is the identity
/// deserializer, while `Tuple(())` covers historical empty-array fixtures.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum UnitBody {
    Object(serde_json::Value),
    Tuple(()),
}

/// Render `&[&str]` flags into the query-string suffix TS6 WebQuery uses
/// for command flags (e.g. `?-uid&-away` for `clientlist`). TS3-era path
/// concatenation (`clientlist-uid-away`) is rejected by TS6 with code 1538
/// (`invalid parameter`); the bare-key query form is what the upstream's
/// own `curl -G` examples produce. An empty slice returns the empty string
/// so callers can unconditionally append it.
fn flag_suffix(flags: &[&str]) -> String {
    if flags.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(flags.iter().map(|f| f.len() + 2).sum::<usize>() + 1);
    out.push('?');
    let mut first = true;
    for flag in flags {
        if !first {
            out.push('&');
        }
        first = false;
        out.push('-');
        out.push_str(flag.trim_start_matches('-'));
    }
    out
}

fn bool_to_int(v: bool) -> &'static str {
    if v { "1" } else { "0" }
}

/// Parameters for [`WebQueryClient::banadd`]. All fields are optional —
/// callers send the subset that applies (e.g. ban-by-IP vs. ban-by-uid).
#[derive(Debug, Default, Clone)]
pub struct BanAddParams<'a> {
    pub ip: Option<&'a str>,
    pub uid: Option<&'a str>,
    pub mytsid: Option<&'a str>,
    pub name: Option<&'a str>,
    pub banreason: Option<&'a str>,
    /// Ban duration in seconds. `Some(0)` is permanent per §7.12; `None`
    /// omits the field entirely (upstream defaults apply).
    pub time: Option<i64>,
}

/// Parameters for a server-group permission upsert
/// ([`WebQueryClient::servergroupaddperm`]). `permsid` is the stable
/// string id; `permnegated` / `permskip` are the tri-state flags TS6
/// server-group permissions carry (PURA-373).
#[derive(Debug, Clone)]
pub struct GroupPermWrite<'a> {
    pub permsid: &'a str,
    pub permvalue: i64,
    pub permnegated: bool,
    pub permskip: bool,
}

/// Selector for [`WebQueryClient::permfind`] — TS `permfind` takes
/// exactly one of `permid` / `permsid`.
#[derive(Debug, Clone, Copy)]
pub enum PermSelector<'a> {
    Id(i64),
    Sid(&'a str),
}

/// Parameters for [`WebQueryClient::privilegekeyadd`]. `token_type` is
/// `0` (server group) or `1` (channel group); `token_id1` / `token_id2`
/// are the target group / channel ids per spec §7.13.
#[derive(Debug, Clone)]
pub struct PrivilegeKeyAddParams<'a> {
    pub token_type: i64,
    pub token_id1: i64,
    pub token_id2: i64,
    pub description: Option<&'a str>,
    pub customset: Option<&'a str>,
}

/// Unwrap the TS6 singleton-wrap shape into a bare JSON value suitable for
/// model decoding. Accepts either `{...}` (the legacy TS3 shape kept around
/// for older fixtures) or `[{...}]` (TS6 wire — `6.0.0-beta9` captured).
/// Multi-element arrays and empty arrays are rejected as `InvalidResponse`
/// to surface wiring mistakes (e.g. a list-shaped command routed through
/// [`WebQueryClient::get_one`] by accident).
fn unwrap_singleton_body(body: serde_json::Value) -> WebQueryResult<serde_json::Value> {
    match body {
        serde_json::Value::Array(mut arr) if arr.len() == 1 => Ok(arr.pop().unwrap()),
        serde_json::Value::Array(arr) if arr.is_empty() => Err(WebQueryError::InvalidResponse(
            "expected single-element body, got empty array".into(),
        )),
        serde_json::Value::Array(arr) => Err(WebQueryError::InvalidResponse(format!(
            "expected single-element body, got {}-element array",
            arr.len()
        ))),
        other => Ok(other),
    }
}

/// Spec §10.5 envelope. Always parsed, regardless of HTTP status.
///
/// `body` is captured as raw [`serde_json::Value`] so the target type's
/// deserializer decides whether `null` (the empty-body success shape used
/// by no-return mutations — `clientkick`, `clientmove`, `clientedit`,
/// `bandel`/`bandelall`, `servernotifyregister`/`servernotifyunregister`,
/// some `sendtextmessage` variants) is acceptable. [`UnitBody`] accepts it;
/// list/struct models reject and surface as [`WebQueryError::InvalidResponse`].
#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    body: Option<serde_json::Value>,
    status: EnvelopeStatus,
}

#[derive(Debug, Deserialize)]
struct EnvelopeStatus {
    code: i64,
    message: String,
}

impl Envelope {
    fn into_body<T>(self) -> WebQueryResult<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        if self.status.code != 0 {
            return Err(WebQueryError::Upstream {
                code: self.status.code,
                message: self.status.message,
            });
        }
        let value = self.body.unwrap_or(serde_json::Value::Null);
        serde_json::from_value(value)
            .map_err(|e| WebQueryError::InvalidResponse(format!("body decode failed: {e}")))
    }
}

/// Pool of WebQuery clients keyed by `server_connection.id` (spec §10.7).
///
/// Phase 1 ships lazy creation only: the dashboard route asks for a client
/// and the pool builds it on first miss. Boot-time pre-population, the 30s
/// `version`-probe health loop, and the `refreshClient` hook on
/// `PUT /servers/:configId` are Phase 2 follow-ups (deferred per the
/// PURA-23 issue scope).
#[derive(Clone)]
pub struct WebQueryPool {
    inner: Arc<RwLock<HashMap<i64, Arc<WebQueryClient>>>>,
    allow_self_signed: bool,
}

impl std::fmt::Debug for WebQueryPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebQueryPool")
            .field("allow_self_signed", &self.allow_self_signed)
            .finish_non_exhaustive()
    }
}

impl WebQueryPool {
    pub fn new(allow_self_signed: bool) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            allow_self_signed,
        }
    }

    /// Insert or replace the client for `connection`. Used by Phase 2 boot
    /// pre-population and by `PUT /servers/:configId` once that route lands.
    pub async fn upsert(
        &self,
        connection: &ServerConnection,
    ) -> WebQueryResult<Arc<WebQueryClient>> {
        let client = Arc::new(WebQueryClient::from_connection(
            connection,
            self.allow_self_signed,
        )?);
        self.inner
            .write()
            .await
            .insert(connection.id, client.clone());
        Ok(client)
    }

    /// Fetch the client for `config_id`, building one from `connection` on
    /// first miss. Returns `None` from the cache only if `connection` is also
    /// absent — callers passing `None` get a `Transport` error so the
    /// dashboard route can map it to `500 "No connection configured for
    /// server config ID"` per §10.7.
    pub async fn get_or_build(
        &self,
        config_id: i64,
        connection: Option<&ServerConnection>,
    ) -> WebQueryResult<Arc<WebQueryClient>> {
        if let Some(existing) = self.inner.read().await.get(&config_id).cloned() {
            return Ok(existing);
        }
        let connection = connection.ok_or_else(|| {
            WebQueryError::transport_other(format!(
                "No connection configured for server config ID {config_id}"
            ))
        })?;
        self.upsert(connection).await
    }

    /// Drop the cached client (if any). Called by `DELETE /servers/:configId`
    /// once that route lands.
    pub async fn remove(&self, config_id: i64) {
        self.inner.write().await.remove(&config_id);
    }
}

#[cfg(test)]
mod tests;
