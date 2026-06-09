//! PURA-144 (WS-6) — `/api/video-sources` REST surface.
//!
//! Operator-facing CRUD over the manager's video-source catalogue. Each
//! row maps 1:1 to a live pipeline on `ts6-media-sidecar`; this module
//! is the integration glue between FE-PAGES (WS-7), the public widget
//! viewer (WS-8) and the sidecar control plane shipped in WS-3
//! (`crates/ts6-media-sidecar/src/control.rs`).
//!
//! Auth: every endpoint requires JWT auth via
//! [`crate::auth::extractors::RequireAuth`]. Per-server access is checked
//! the same way as the rest of `routes/control/*` — admins skip the check,
//! moderators/viewers need a `server_user_grant` row.
//!
//! SSRF: the operator-supplied `url` is validated by [`ts6_ssrf`] BEFORE
//! the sidecar is asked to start a pipeline. The sidecar runs its own
//! validator too (defence in depth); both share the same allow-list and
//! the same crate path so a tightening on one side surfaces on the other.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::json;
use ts6_manager_shared::video_sources as wire;
use ts6_ssrf::{SsrfError, is_url_allowed};

use crate::app_state::AppState;
use crate::auth::extractors::RequireAuth;
use crate::control::sidecar::{
    KNOWN_PRESETS, SidecarClient, SidecarClientError, StartSourceRequest, StopSourceRequest,
};
use crate::db::ClassifyDbResult;
use crate::repos::{server_connections, video_sources};
use crate::ws::topic::{Topic, TopicKind};

use super::access;
use super::{ErrorBody, err, err_body};

/// Re-export the canonical wire row so callers inside the server crate keep
/// the existing `VideoSourceView` import path while the type definition
/// lives in `ts6_manager_shared::video_sources`. The FE imports
/// `ts6_manager_shared` directly.
pub use ts6_manager_shared::video_sources::{
    CreateVideoSourceRequest as CreateRequest, VideoSourceView,
};

/// Mount the video-source sub-router.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/video-sources", post(create).get(list))
        .route("/api/video-sources/{id}", get(detail).delete(delete))
}

// ---------------------------------------------------------------------------
// Wire DTOs — types live in `ts6_manager_shared::video_sources` (re-exported
// above so callers inside this crate keep their existing import paths). The
// Dioxus FE consumes the same shapes through `ts6_manager_shared` directly,
// keeping the WS-6 wire contract in lock-step.
// ---------------------------------------------------------------------------

/// Convert a sqlx-backed [`video_sources::VideoSource`] into the wire row.
/// Lives in the server crate because the source `VideoSource` carries
/// `sqlx`/`chrono` flavour the shared crate intentionally doesn't depend on.
fn view_from_row(row: video_sources::VideoSource) -> VideoSourceView {
    // The sidecar's TrackDescriptor uses the source_id as the moq-lite
    // namespace; the video / audio track names are hardcoded by
    // `pipeline.rs` so the reference player works without configuration.
    let track = wire::TrackDescriptorView {
        namespace: row.sourceId.clone(),
        video: "video".to_string(),
        audio: "audio".to_string(),
    };
    VideoSourceView {
        id: row.id,
        source_id: row.sourceId,
        label: row.label,
        url: row.url,
        preset: row.preset,
        server_id: row.serverConfigId,
        status: row.status,
        track,
        created_by_user_id: row.createdByUserId,
        created_at: row.createdAt,
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `POST /api/video-sources` — validate the URL, start the pipeline on
/// the sidecar, persist the row.
async fn create(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Json(req): Json<CreateRequest>,
) -> Result<(StatusCode, Json<VideoSourceView>), Response> {
    let sidecar = sidecar_or_503(&state)?;

    // 1. Validate basic shape.
    let url = req.url.trim().to_string();
    if url.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "url is required"));
    }
    let label = req.label.trim().to_string();
    if label.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "label is required"));
    }
    if label.len() > 256 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "label must be ≤256 characters",
        ));
    }
    let preset = match req.preset.as_deref() {
        None | Some("") => "720p".to_string(),
        Some(p) if KNOWN_PRESETS.contains(&p) => p.to_string(),
        Some(p) => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                &format!(
                    "preset must be one of {} (got `{p}`)",
                    KNOWN_PRESETS.join(", ")
                ),
            ));
        }
    };

    // 2. Resolve the target server.
    let server_id = resolve_server_id(&state, req.server_id).await?;
    // Access check: admin skips; otherwise the user needs a grant on
    // this server (same posture as `routes/control/*`).
    access::check_write(&state, &auth, server_id).await?;

    // 3. SSRF pre-flight. Defence in depth — the sidecar runs the same
    // validator before spawning FFmpeg.
    if let Err(e) = is_url_allowed(&url, state.ssrf_resolver.as_ref()).await {
        return Err(translate_ssrf_error(e));
    }

    // 4. Forward to sidecar.
    let started = sidecar
        .start_source(&StartSourceRequest {
            url: url.clone(),
            preset: Some(preset.clone()),
        })
        .await
        .map_err(translate_sidecar_error)?;

    // 5. Persist.
    //
    // R8 boundary — `classify_db()` maps any underlying `surrealdb::Error`
    // onto the three named storage-full boundaries (write-failure /
    // transaction-conflict / capacity-pressure). Transaction conflicts
    // surface as `409 Conflict` with a retry-positive body; capacity
    // pressure surfaces as `507 Insufficient Storage`; everything else
    // ends up as `500 Internal Server Error` with a static body. The
    // underlying SurrealDB message text never crosses the wire — only the
    // tracing log on the IntoResponse impl preserves it for operators.
    let row = video_sources::insert(
        &state.db,
        video_sources::NewVideoSource {
            sourceId: started.source_id.clone(),
            label,
            url,
            preset,
            serverConfigId: server_id,
            createdByUserId: Some(auth.id),
            status: "starting".into(),
        },
    )
    .await
    .classify_db()
    .map_err(|e| {
        tracing::warn!(
            boundary = ?e.boundary,
            source_id = %started.source_id,
            error = %e.source,
            "video_source insert failed; orphaning sidecar pipeline"
        );
        e.into_response()
    })?;

    let view = view_from_row(row);

    // 6. WS push — immediate `starting` event so any subscribed FE tab
    // sees the new row without a refetch.
    let topic = Topic::new(server_id, TopicKind::VideoSources);
    state
        .ws_hub
        .publish(
            topic,
            "video_source:created",
            serde_json::to_value(&view).unwrap_or(json!({})),
        )
        .await;

    Ok((StatusCode::CREATED, Json(view)))
}

/// `GET /api/video-sources` — list rows the caller is allowed to see.
/// Admin sees everything; non-admin sees rows on servers they have a
/// grant for.
async fn list(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
) -> Result<Json<Vec<VideoSourceView>>, Response> {
    let all = video_sources::list_all(&state.db).await.map_err(|e| {
        tracing::warn!(error = %e, "video_sources list query failed");
        err(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
    })?;

    let mut out = Vec::with_capacity(all.len());
    for row in all {
        // Admin shortcut: skip per-row grant lookups.
        let allowed = if auth.is_admin() {
            true
        } else {
            access::check_read(&state, &auth, row.serverConfigId)
                .await
                .is_ok()
        };
        if allowed {
            out.push(view_from_row(row));
        }
    }
    Ok(Json(out))
}

/// `GET /api/video-sources/{id}` — fetch a single row by primary key.
async fn detail(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(id): Path<i64>,
) -> Result<Json<VideoSourceView>, Response> {
    let row = video_sources::find_by_id(&state.db, id)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, id, "video_source find_by_id failed");
            err(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
        })?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Not found"))?;
    access::check_read(&state, &auth, row.serverConfigId).await?;
    Ok(Json(view_from_row(row)))
}

/// `DELETE /api/video-sources/{id}` — stop the pipeline on the sidecar,
/// then delete the row. Idempotent: a 404 from the sidecar (pipeline
/// already gone) is treated as success so the operator can clean up a
/// stale row after a sidecar restart.
async fn delete(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(id): Path<i64>,
) -> Result<StatusCode, Response> {
    let sidecar = sidecar_or_503(&state)?;
    let row = video_sources::find_by_id(&state.db, id)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, id, "video_source find_by_id failed");
            err(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
        })?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Not found"))?;
    access::check_write(&state, &auth, row.serverConfigId).await?;

    // The sidecar's `stop_source` already treats 404 as success — see
    // `SidecarClient::stop_source` — so this call stays idempotent in
    // the face of a previously-orphaned pipeline.
    if let Err(e) = sidecar
        .stop_source(&StopSourceRequest {
            source_id: row.sourceId.clone(),
        })
        .await
    {
        tracing::warn!(
            error = %e,
            source_id = %row.sourceId,
            "sidecar stop_source failed during DELETE; deleting row anyway"
        );
    }
    video_sources::delete_by_id(&state.db, id)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, id, "video_source delete failed");
            err(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
        })?;

    // WS push so the FE drops the row from its list without a refetch.
    let topic = Topic::new(row.serverConfigId, TopicKind::VideoSources);
    state
        .ws_hub
        .publish(
            topic,
            "video_source:deleted",
            json!({ "id": id, "source_id": row.sourceId }),
        )
        .await;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

// The `Err` variant is an axum `Response` (~136 B). Every handler in this
// module already propagates `Result<_, Response>` via `?`, so boxing here
// would force unwrapping at every caller. Match the established pattern.
#[allow(clippy::result_large_err)]
fn sidecar_or_503(state: &AppState) -> Result<&SidecarClient, Response> {
    state.sidecar.as_ref().ok_or_else(|| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "Video sidecar is not configured (SIDECAR_URL unset).",
        )
    })
}

async fn resolve_server_id(state: &AppState, requested: Option<i64>) -> Result<i64, Response> {
    if let Some(id) = requested {
        // Confirm the server exists; otherwise 404.
        let exists = server_connections::find_by_id(&state.db, id)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, server_id = id, "server_connections lookup failed");
                err(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
            })?
            .is_some();
        if !exists {
            return Err(err(StatusCode::NOT_FOUND, "server_id not found"));
        }
        return Ok(id);
    }
    // No explicit server_id — auto-pick when the operator has exactly
    // one server. Multi-server deployments must always pass it.
    let mut rows = server_connections::list(&state.db).await.map_err(|e| {
        tracing::warn!(error = %e, "server_connections list failed");
        err(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
    })?;
    rows.retain(|r| r.enabled);
    match rows.len() {
        0 => Err(err(
            StatusCode::BAD_REQUEST,
            "No enabled server connections — set up a server first.",
        )),
        1 => Ok(rows[0].id),
        _ => Err(err(
            StatusCode::BAD_REQUEST,
            "server_id is required when the operator has multiple servers.",
        )),
    }
}

fn translate_ssrf_error(e: SsrfError) -> Response {
    err_body(
        StatusCode::BAD_REQUEST,
        ErrorBody {
            error: "URL rejected by SSRF policy".into(),
            code: None,
            details: Some(e.to_string()),
        },
    )
}

fn translate_sidecar_error(e: SidecarClientError) -> Response {
    match e {
        SidecarClientError::Transport(detail) => err_body(
            StatusCode::BAD_GATEWAY,
            ErrorBody {
                error: "Sidecar unreachable".into(),
                code: None,
                details: Some(detail),
            },
        ),
        SidecarClientError::Upstream { status, body } => {
            // Pass through 4xx from the sidecar (validation, conflict)
            // verbatim so the FE can surface the same string the
            // operator would see by curl'ing the sidecar directly.
            // 5xx maps to 502 — distinct from "manager error" so an
            // alert can fire on the right side.
            let outbound = if status.is_client_error() {
                status
            } else {
                StatusCode::BAD_GATEWAY
            };
            err_body(
                outbound,
                ErrorBody {
                    error: "Sidecar refused request".into(),
                    code: Some(status.as_u16() as i64),
                    details: Some(body),
                },
            )
        }
        SidecarClientError::Malformed(detail) => err_body(
            StatusCode::BAD_GATEWAY,
            ErrorBody {
                error: "Sidecar returned malformed response".into(),
                code: None,
                details: Some(detail),
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    //! Integration tests for the `/api/video-sources` surface.
    //!
    //! Spins up a mock `ts6-media-sidecar` on a random localhost port
    //! (returning canned WS-3 responses), points `AppState.sidecar` at
    //! it, then exercises the route via `Router::oneshot`.
    //!
    //! Covers WS-6's acceptance:
    //! - CRUD round-trip against the mocked sidecar.
    //! - SSRF rejection at the manager layer (private IP literal).
    //! - DELETE is idempotent when the sidecar has already lost the
    //!   source (returns 404 → manager treats as success).

    use super::*;
    use axum::body::Body;
    use axum::http::{HeaderValue, Method, Request, StatusCode as AxStatus};
    use axum::routing::{get, post};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;
    use tower::ServiceExt;

    use crate::app_state::AppState;
    use crate::auth::{jwt, password};
    use crate::control::sidecar::SidecarClient;
    use crate::crypto;
    use crate::db::{connect_in_memory, migrations};
    use crate::repos::server_connections::{NewServerConnection, ServerConnection, insert};
    use crate::repos::users;
    use crate::webquery::WebQueryPool;
    use crate::ws::Hub;

    #[derive(Clone, Default)]
    struct MockSidecar {
        start_calls: Arc<Mutex<Vec<Value>>>,
        stop_calls: Arc<Mutex<Vec<Value>>>,
        next_source_id: Arc<Mutex<u64>>,
        stop_returns_not_found: Arc<Mutex<bool>>,
    }

    async fn handle_start(
        axum::extract::State(state): axum::extract::State<MockSidecar>,
        axum::Json(body): axum::Json<Value>,
    ) -> impl axum::response::IntoResponse {
        state.start_calls.lock().unwrap().push(body);
        let mut n = state.next_source_id.lock().unwrap();
        *n += 1;
        let source_id = format!("mock-src-{}", *n);
        (
            AxStatus::CREATED,
            axum::Json(json!({
                "source_id": source_id,
                "track": {
                    "namespace": source_id,
                    "video": "video",
                    "audio": "audio"
                }
            })),
        )
    }

    async fn handle_stop(
        axum::extract::State(state): axum::extract::State<MockSidecar>,
        axum::Json(body): axum::Json<Value>,
    ) -> AxStatus {
        state.stop_calls.lock().unwrap().push(body);
        if *state.stop_returns_not_found.lock().unwrap() {
            AxStatus::NOT_FOUND
        } else {
            AxStatus::NO_CONTENT
        }
    }

    async fn handle_stats(
        axum::extract::State(_state): axum::extract::State<MockSidecar>,
    ) -> axum::Json<Value> {
        axum::Json(
            json!({"uptime_s": 0, "active_sessions": 0, "lifetime_sessions": 0, "registered_broadcasts": [], "sources": []}),
        )
    }

    async fn boot_mock_sidecar() -> (SidecarClient, MockSidecar) {
        let mock = MockSidecar::default();
        *mock.next_source_id.lock().unwrap() = 0;
        let app = axum::Router::new()
            .route("/source", post(handle_start))
            .route("/source/stop", post(handle_stop))
            .route("/stats", get(handle_stats))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (SidecarClient::new(format!("http://127.0.0.1:{port}")), mock)
    }

    async fn fresh_state_with_sidecar() -> (AppState, MockSidecar) {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        crypto::init("test-seed-pura-144-video-sources");
        let (sidecar, mock) = boot_mock_sidecar().await;
        let control = crate::control::ControlBackendPool::new(false, db.clone());
        let state = AppState {
            db,
            jwt_secret: Arc::new(b"test-secret-bytes-please-32-or-more".to_vec()),
            jwt_access_expiry: Duration::from_secs(900),
            jwt_refresh_expiry: Duration::from_secs(7 * 24 * 3600),
            setup_lock: Arc::new(tokio::sync::Mutex::new(())),
            webquery: WebQueryPool::new(false),
            control,
            ws_hub: Hub::new(),
            widget_cache: crate::widgets::WidgetCache::new(),
            music_bots: crate::music_bots::MusicBotService::default_for_tests(),
            sidecar: Some(sidecar),
            // Use the mock resolver — the test URLs use IP literals so
            // SSRF lookups never hit DNS, but the resolver field is
            // mandatory on AppState.
            ssrf_resolver: Arc::new(ts6_ssrf::MockResolver::new()),
            moq_public_url: None,
            yt_cookie: std::sync::Arc::new(std::sync::RwLock::new(None)),
            yt_api_key: std::sync::Arc::new(std::sync::RwLock::new(None)),
            data_dir: std::path::PathBuf::from("./data"),
            trusted_proxy_hops: 0,
        };
        (state, mock)
    }

    async fn seed_admin_token(state: &AppState) -> String {
        let pw = "Hunter2!ok".to_string();
        let hash = tokio::task::spawn_blocking(move || password::hash_new(&pw))
            .await
            .unwrap()
            .unwrap();
        let row = users::insert(
            &state.db,
            users::NewUser {
                username: "admin".into(),
                passwordHash: hash,
                displayName: "Admin".into(),
                role: "admin".into(),
                enabled: true,
            },
        )
        .await
        .unwrap();
        jwt::mint_access(
            row.id,
            &row.username,
            &row.role,
            state.jwt_access_expiry,
            &state.jwt_secret,
        )
        .unwrap()
    }

    async fn seed_server(state: &AppState) -> ServerConnection {
        let new = NewServerConnection {
            name: "Mock".into(),
            host: "127.0.0.1".into(),
            webqueryPort: 10080,
            apiKey: crypto::seal("API-KEY").unwrap(),
            useHttps: false,
            sshPort: 10022,
            sshUsername: None,
            sshPassword: None,
            queryBotChannel: None,
            queryBotNickname: None,
            sshBotNickname: None,
            enabled: true,
            controlPath: None,
            sshAuthMethod: None,
            sshPrivateKey: None,
            sshKeyAgentSocket: None,
            sshHostKeyFingerprint: None,
        };
        insert(&state.db, new).await.unwrap()
    }

    fn app(state: AppState) -> axum::Router {
        axum::Router::new().merge(super::router()).with_state(state)
    }

    fn auth_header(token: &str) -> HeaderValue {
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap()
    }

    async fn read_json<T: serde::de::DeserializeOwned>(resp: axum::http::Response<Body>) -> T {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "expected JSON, got {:?}: {e}",
                String::from_utf8_lossy(&bytes)
            )
        })
    }

    // ---------------------------------------------------------------
    // CRUD round-trip
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn create_then_list_then_get_then_delete() {
        let (state, mock) = fresh_state_with_sidecar().await;
        let token = seed_admin_token(&state).await;
        let server = seed_server(&state).await;

        // POST /api/video-sources
        let create_body = json!({
            "url": "http://93.184.216.34/stream.m3u8",
            "label": "Lobby cam",
            "preset": "720p",
            "server_id": server.id
        });
        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/video-sources")
                    .header("authorization", auth_header(&token))
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&create_body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), AxStatus::CREATED);
        let view: VideoSourceView = read_json(resp).await;
        assert_eq!(view.source_id, "mock-src-1");
        assert_eq!(view.label, "Lobby cam");
        assert_eq!(view.preset, "720p");
        assert_eq!(view.server_id, server.id);
        assert_eq!(view.track.namespace, "mock-src-1");
        assert_eq!(view.track.video, "video");
        assert_eq!(view.track.audio, "audio");
        // Sidecar saw the start call. Scope the std::sync::MutexGuard
        // explicitly so it can never be held across the awaits that follow.
        {
            let starts = mock.start_calls.lock().unwrap();
            assert_eq!(starts.len(), 1);
            assert_eq!(starts[0]["url"], "http://93.184.216.34/stream.m3u8");
            assert_eq!(starts[0]["preset"], "720p");
        }

        // GET /api/video-sources (list)
        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/video-sources")
                    .header("authorization", auth_header(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), AxStatus::OK);
        let list: Vec<VideoSourceView> = read_json(resp).await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, view.id);

        // GET /api/video-sources/{id}
        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .uri(format!("/api/video-sources/{}", view.id))
                    .header("authorization", auth_header(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), AxStatus::OK);
        let one: VideoSourceView = read_json(resp).await;
        assert_eq!(one.id, view.id);

        // DELETE /api/video-sources/{id}
        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(format!("/api/video-sources/{}", view.id))
                    .header("authorization", auth_header(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), AxStatus::NO_CONTENT);
        assert_eq!(mock.stop_calls.lock().unwrap().len(), 1);

        // List is now empty.
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .uri("/api/video-sources")
                    .header("authorization", auth_header(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let list: Vec<VideoSourceView> = read_json(resp).await;
        assert!(list.is_empty(), "row should be gone after DELETE");
    }

    // ---------------------------------------------------------------
    // SSRF rejection at the manager layer.
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn create_rejects_private_ip_at_manager_layer() {
        let (state, mock) = fresh_state_with_sidecar().await;
        let token = seed_admin_token(&state).await;
        let server = seed_server(&state).await;

        // 10.0.0.0/8 is RFC1918 — must be blocked by the SSRF
        // validator before the sidecar is contacted.
        let body = json!({
            "url": "http://10.0.0.5/stream.m3u8",
            "label": "Internal cam",
            "server_id": server.id
        });
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/video-sources")
                    .header("authorization", auth_header(&token))
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), AxStatus::BAD_REQUEST);
        // Sidecar must not have been contacted.
        assert!(
            mock.start_calls.lock().unwrap().is_empty(),
            "SSRF rejection must short-circuit before the sidecar call"
        );
    }

    // ---------------------------------------------------------------
    // Idempotent DELETE.
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn delete_is_idempotent_when_sidecar_lost_source() {
        let (state, mock) = fresh_state_with_sidecar().await;
        let token = seed_admin_token(&state).await;
        let server = seed_server(&state).await;

        // Create one source so the row exists.
        let create_body = json!({
            "url": "http://93.184.216.34/stream.m3u8",
            "label": "Orphan cam",
            "server_id": server.id
        });
        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/video-sources")
                    .header("authorization", auth_header(&token))
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&create_body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), AxStatus::CREATED);
        let view: VideoSourceView = read_json(resp).await;

        // Sidecar restart simulation: from now on /source/stop returns
        // 404 (pipeline no longer registered).
        *mock.stop_returns_not_found.lock().unwrap() = true;

        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(format!("/api/video-sources/{}", view.id))
                    .header("authorization", auth_header(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            AxStatus::NO_CONTENT,
            "DELETE must succeed even when sidecar lost the source"
        );
        // Row is gone — second DELETE on the same id is now a 404
        // (the DB row no longer exists), which is the correct shape.
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(format!("/api/video-sources/{}", view.id))
                    .header("authorization", auth_header(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), AxStatus::NOT_FOUND);
    }

    // ---------------------------------------------------------------
    // Sidecar-not-configured posture.
    // ---------------------------------------------------------------

    // ---------------------------------------------------------------
    // WS push — POST emits a `video_source:created` envelope on the
    // per-server `video_sources` topic.
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn create_pushes_video_source_created_on_ws_topic() {
        use crate::ws::auth::UserPrincipal;
        use crate::ws::topic::{Topic, TopicKind};

        let (state, _mock) = fresh_state_with_sidecar().await;
        let token = seed_admin_token(&state).await;
        let server = seed_server(&state).await;

        // Subscribe to the topic before we POST so the broadcast is
        // captured. Use an admin principal so the ACL passes.
        let principal = crate::ws::auth::Principal::User(UserPrincipal {
            user_id: 1,
            username: "admin".into(),
            role: "admin".into(),
            is_admin: true,
            is_at_least_moderator: true,
        });
        let topic = Topic::new(server.id, TopicKind::VideoSources);
        let mut sub = state
            .ws_hub
            .subscribe(&state.db, &principal, topic, None)
            .await
            .expect("admin can subscribe");

        let body = json!({
            "url": "http://93.184.216.34/stream.m3u8",
            "label": "Stream",
            "server_id": server.id
        });
        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/video-sources")
                    .header("authorization", auth_header(&token))
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), AxStatus::CREATED);

        let env = tokio::time::timeout(Duration::from_millis(500), sub.receiver.recv())
            .await
            .expect("envelope must arrive within 500ms")
            .expect("recv must succeed");
        assert_eq!(env.kind, "video_source:created");
        assert_eq!(env.data["source_id"], "mock-src-1");
        assert_eq!(env.data["label"], "Stream");
        assert_eq!(env.data["server_id"], server.id);
    }

    #[tokio::test]
    async fn create_returns_503_when_sidecar_unconfigured() {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        crypto::init("test-seed-pura-144-no-sidecar");
        let control = crate::control::ControlBackendPool::new(false, db.clone());
        let state = AppState {
            db,
            jwt_secret: Arc::new(b"test-secret-bytes-please-32-or-more".to_vec()),
            jwt_access_expiry: Duration::from_secs(900),
            jwt_refresh_expiry: Duration::from_secs(7 * 24 * 3600),
            setup_lock: Arc::new(tokio::sync::Mutex::new(())),
            webquery: WebQueryPool::new(false),
            control,
            ws_hub: Hub::new(),
            widget_cache: crate::widgets::WidgetCache::new(),
            music_bots: crate::music_bots::MusicBotService::default_for_tests(),
            sidecar: None,
            ssrf_resolver: Arc::new(ts6_ssrf::MockResolver::new()),
            moq_public_url: None,
            yt_cookie: std::sync::Arc::new(std::sync::RwLock::new(None)),
            yt_api_key: std::sync::Arc::new(std::sync::RwLock::new(None)),
            data_dir: std::path::PathBuf::from("./data"),
            trusted_proxy_hops: 0,
        };
        let token = seed_admin_token(&state).await;
        let server = seed_server(&state).await;
        let body = json!({
            "url": "http://93.184.216.34/stream.m3u8",
            "label": "x",
            "server_id": server.id
        });
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/video-sources")
                    .header("authorization", auth_header(&token))
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), AxStatus::SERVICE_UNAVAILABLE);
    }
}
