//! `GET /api/servers/:configId/vs/:sid/dashboard` (spec §7.19) — PURA-23,
//! re-pointed onto [`crate::control::ControlBackend`] in PURA-78.
//!
//! Auth-gated by [`RequireServerAccess`] (spec §7.19 `Y+access`): admin
//! bypasses grants; every other role needs a `server_user_grant` row.
//! Missing `server_connection` → `404`.
//!
//! The handler:
//! 1. Looks up `server_connections.id == configId`. Missing → `404`.
//! 2. Pulls / lazily creates the [`crate::control::ControlBackend`] for
//!    the connection from [`crate::app_state::AppState::control`]. The
//!    per-server `controlPath` flag picks WebQuery vs. SSHBridge —
//!    this handler stays oblivious.
//! 3. Issues `serverinfo`, `clientlist`, `channellist`, and
//!    `serverrequestconnectioninfo` against `:sid` *in parallel* (spec
//!    §7.19 mandates parallel dispatch).
//! 4. Aggregates into a [`DashboardData`] per spec §7.19.1.
//!
//! Errors propagate as:
//! - Missing connection row → `404 {"error": "Not found"}` (§7.0.2).
//! - Bad integer URL params → `400 {"error": "Invalid <name>: must be a
//!   number"}` (§7.0.1).
//! - Backend upstream non-zero status → `502 {"error": "TeamSpeak API
//!   Error", "code": <int>, "details": "<message>"}` (§7.0.2).
//! - Transport / TLS / decrypt / SSH-auth failure → `502` with
//!   `code: -1` and the error message in `details` (§10.5).

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::{Deserialize, Serialize};
use ts6_manager_shared::dashboard::{BandwidthSnapshot, DashboardData};

use crate::app_state::AppState;
use crate::auth::extractors::RequireServerAccess;
use crate::control::{ControlBackend, ControlBackendError, ControlResult};
use crate::repos::server_connections;

use super::models;

/// Fan-out the four §7.19 ServerQuery calls in parallel against `client`
/// and reduce them into a [`DashboardData`] snapshot. Shared between the
/// HTTP handler and the WS tick republisher (PURA-81) so both report the
/// exact same payload.
pub(crate) async fn fetch_dashboard(
    client: &dyn ControlBackend,
    sid: i64,
) -> ControlResult<DashboardData> {
    let (info, clients, channels, conn_info) = tokio::try_join!(
        client.serverinfo(sid),
        client.clientlist(sid),
        client.channellist(sid),
        client.server_connection_info(sid),
    )?;
    Ok(aggregate(info, clients, channels, conn_info))
}

/// Build the dashboard sub-router. Caller mounts it at
/// `/api/servers/{configId}/vs/{sid}/dashboard`.
pub fn router() -> Router<AppState> {
    Router::new().route("/api/servers/{configId}/vs/{sid}/dashboard", get(handler))
}

/// `{ "error": ..., "details"?: ..., "code"?: ... }` — spec §7.0.2 wire shape.
#[derive(Debug, Serialize, Deserialize)]
struct ErrorBody {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<String>,
}

fn err_body(status: StatusCode, body: ErrorBody) -> Response {
    (status, Json(body)).into_response()
}

fn translate_control_error(err: ControlBackendError) -> Response {
    let status = err.http_status();
    let body = match status {
        StatusCode::BAD_GATEWAY => ErrorBody {
            error: "TeamSpeak API Error".into(),
            code: Some(err.upstream_code()),
            details: Some(err.upstream_message()),
        },
        _ => ErrorBody {
            error: "Internal server error".into(),
            code: None,
            details: None,
        },
    };
    err_body(status, body)
}

#[derive(Debug, Deserialize)]
pub struct DashboardPath {
    #[serde(rename = "configId")]
    pub config_id: i64,
    pub sid: i64,
}

async fn handler(
    State(state): State<AppState>,
    RequireServerAccess { connection, .. }: RequireServerAccess,
    Path(params): Path<DashboardPath>,
) -> Result<Json<DashboardData>, Response> {
    let DashboardPath { config_id, sid } = params;

    let client = state
        .control
        .get_or_build(config_id, Some(&connection))
        .await
        .map_err(translate_control_error)?;

    // Spec §7.19: issue the four upstream calls in parallel. Trait
    // dispatch routes through whichever backend `controlPath` selected
    // for this connection.
    let dashboard = fetch_dashboard(client.as_ref(), sid)
        .await
        .map_err(translate_control_error)?;

    // PURA-222 — bump `lastSeenAt` so the `/servers` index renders the
    // operator's "last successful WebQuery probe" column truthfully. Fire-
    // and-forget: the dashboard response shouldn't fail just because the
    // bookkeeping write hiccuped, and we already hold a successful
    // `dashboard` value so the operator-facing 200 is committed.
    let db = state.db.clone();
    tokio::spawn(async move {
        if let Err(e) = server_connections::touch_last_seen(&db, config_id).await {
            tracing::warn!(err = %e, config_id, "dashboard: touch_last_seen failed");
        }
    });

    Ok(Json(dashboard))
}

/// Reduce the four typed responses into the §7.19.1 wire shape. ServerQuery
/// slots (`client_type == 1`) are excluded from `onlineUsers` per the spec's
/// explicit "MUST" sentence.
fn aggregate(
    info: models::ServerInfo,
    clients: Vec<models::ClientEntry>,
    channels: Vec<models::ChannelEntry>,
    conn_info: models::ConnectionInfo,
) -> DashboardData {
    let online_users = clients
        .iter()
        .filter(|c| c.client_type == 0)
        .count()
        .try_into()
        .unwrap_or(u32::MAX);

    DashboardData {
        server_name: info.virtualserver_name,
        platform: info.virtualserver_platform,
        version: info.virtualserver_version,
        online_users,
        max_clients: info.virtualserver_maxclients.try_into().unwrap_or(u32::MAX),
        uptime: info.virtualserver_uptime.try_into().unwrap_or(0),
        channel_count: channels.len().try_into().unwrap_or(u32::MAX),
        bandwidth: BandwidthSnapshot {
            incoming: conn_info
                .connection_bandwidth_received_last_second_total
                .try_into()
                .unwrap_or(0),
            outgoing: conn_info
                .connection_bandwidth_sent_last_second_total
                .try_into()
                .unwrap_or(0),
        },
        packetloss: info.virtualserver_total_packetloss_total,
        ping: info.virtualserver_total_ping,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{jwt, password};
    use crate::crypto;
    use crate::db::{connect_in_memory, migrations};
    use crate::repos::server_connections::NewServerConnection;
    use crate::repos::{server_user_grants, users};
    use crate::webquery::WebQueryPool;
    use crate::ws::Hub;
    use axum::body::Body;
    use axum::http::{HeaderValue, Method, Request};
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use std::time::Duration;
    use tower::ServiceExt;
    use ts6_manager_shared::auth::ErrorResponse;

    async fn fresh_state() -> AppState {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        crypto::init("test-seed-dashboard-server-access");
        let control = crate::control::ControlBackendPool::new(false, db.clone());
        AppState {
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
            bug_reports: crate::bug_reports::unconfigured_sink(),
        }
    }

    async fn seed_user_token(state: &AppState, name: &str, role: &str) -> (i64, String) {
        let pw = "Hunter2!ok".to_string();
        let hash = tokio::task::spawn_blocking(move || password::hash_new(&pw))
            .await
            .unwrap()
            .unwrap();
        let row = users::insert(
            &state.db,
            users::NewUser {
                username: name.into(),
                passwordHash: hash,
                displayName: name.into(),
                role: role.into(),
                enabled: true,
            },
        )
        .await
        .unwrap();
        let token = jwt::mint_access(
            row.id,
            &row.username,
            &row.role,
            state.jwt_access_expiry,
            &state.jwt_secret,
        )
        .unwrap();
        (row.id, token)
    }

    async fn seed_server(state: &AppState) -> i64 {
        crate::repos::server_connections::insert(
            &state.db,
            NewServerConnection {
                name: "S".into(),
                host: "ts.example.com".into(),
                webqueryPort: 10080,
                apiKey: crypto::seal("k").unwrap(),
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
            },
        )
        .await
        .unwrap()
        .id
    }

    fn app(state: AppState) -> Router {
        super::router().with_state(state)
    }

    async fn get_dashboard(
        state: AppState,
        token: Option<&str>,
        config_id: i64,
    ) -> axum::http::Response<Body> {
        let mut req = Request::builder()
            .method(Method::GET)
            .uri(format!("/api/servers/{config_id}/vs/1/dashboard"));
        if let Some(token) = token {
            req = req.header(
                "authorization",
                HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
            );
        }
        app(state)
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn read_error(resp: axum::http::Response<Body>) -> ErrorResponse {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn dashboard_viewer_without_grant_is_403() {
        let state = fresh_state().await;
        let (_id, token) = seed_user_token(&state, "viewer", "viewer").await;
        let sid = seed_server(&state).await;
        let resp = get_dashboard(state, Some(&token), sid).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            read_error(resp).await.error,
            ts6_manager_shared::auth::auth_error_strings::INSUFFICIENT_PERMS
        );
    }

    #[tokio::test]
    async fn dashboard_viewer_with_grant_passes_the_access_gate() {
        // Grant check happens before the WebQuery fan-out. A granted
        // viewer must not 403; the subsequent 502 is the mock-less
        // upstream (no live TS) and proves the extractor let them through.
        let state = fresh_state().await;
        let (uid, token) = seed_user_token(&state, "viewer", "viewer").await;
        let sid = seed_server(&state).await;
        server_user_grants::insert(&state.db, uid, sid)
            .await
            .unwrap();
        let resp = get_dashboard(state, Some(&token), sid).await;
        assert_ne!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "granted viewer must pass RequireServerAccess"
        );
        assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_ne!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn dashboard_admin_passes_the_access_gate_without_a_grant() {
        let state = fresh_state().await;
        let (_id, token) = seed_user_token(&state, "alice", "admin").await;
        let sid = seed_server(&state).await;
        let resp = get_dashboard(state, Some(&token), sid).await;
        assert_ne!(resp.status(), StatusCode::FORBIDDEN);
        assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_ne!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn dashboard_missing_server_is_404() {
        let state = fresh_state().await;
        let (_id, token) = seed_user_token(&state, "alice", "admin").await;
        let resp = get_dashboard(state, Some(&token), 9999).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(read_error(resp).await.error, "Not found");
    }

    #[test]
    fn aggregate_excludes_serverquery_slots_from_online_users() {
        let info = models::ServerInfo {
            virtualserver_name: "Alpha".into(),
            virtualserver_platform: "Linux".into(),
            virtualserver_version: "3.13.7".into(),
            virtualserver_maxclients: 32,
            virtualserver_uptime: 100,
            virtualserver_total_packetloss_total: 0.0,
            virtualserver_total_ping: 1.0,
        };
        let clients = vec![
            models::ClientEntry {
                clid: 1,
                client_type: 0,
                client_nickname: "Alice".into(),
                ..Default::default()
            },
            models::ClientEntry {
                clid: 2,
                client_type: 0,
                client_nickname: "Bob".into(),
                ..Default::default()
            },
            models::ClientEntry {
                clid: 99,
                client_type: 1,
                client_nickname: "serveradmin".into(),
                ..Default::default()
            },
        ];
        let channels = vec![
            models::ChannelEntry {
                cid: 1,
                channel_name: "A".into(),
                ..Default::default()
            },
            models::ChannelEntry {
                cid: 2,
                channel_name: "B".into(),
                ..Default::default()
            },
        ];
        let conn_info = models::ConnectionInfo {
            connection_bandwidth_received_last_second_total: 100,
            connection_bandwidth_sent_last_second_total: 200,
        };

        let dd = aggregate(info, clients, channels, conn_info);
        assert_eq!(dd.online_users, 2, "ServerQuery slot must be excluded");
        assert_eq!(dd.channel_count, 2);
        assert_eq!(dd.max_clients, 32);
        assert_eq!(dd.bandwidth.incoming, 100);
        assert_eq!(dd.bandwidth.outgoing, 200);
    }
}
