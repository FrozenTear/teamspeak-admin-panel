//! Spec §7.5 — `/api/servers` (list + create). PURA-22 slice 5.
//!
//! Phase 1 ships the two endpoints needed to unblock the FE selector:
//!
//! - `GET /api/servers` — `Y` (any authenticated user). Admins see every
//!   row; other roles see only the rows joined via `server_user_grant`.
//!   `apiKey` is omitted from every response by construction (spec §7.5);
//!   each row is augmented with `hasSshCredentials: !!sshUsername`.
//! - `POST /api/servers` — `Y+admin`. Body fields per spec §7.5;
//!   `apiKey` and `sshPassword` are AES-256-GCM-sealed before insert
//!   (spec §6.3).
//!
//! The per-server `PATCH/DELETE/test` and the dashboard count endpoint
//! land in their own follow-up tickets — see PURA-22 § "Out of scope".
//!
//! Security lenses applied:
//! - **AuthZ**: `RequireAdmin` extractor on `POST` so a moderator/viewer
//!   cannot smuggle a hostile server into the pool.
//! - **Cryptography**: seal happens before write; failure to seal aborts
//!   the request (no plaintext credential ever lands on disk).
//! - **Data protection**: response shape (`ServerSummary`) has no
//!   `apiKey` field — wire contract enforced by the type system.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use ts6_manager_shared::auth::ErrorResponse;
use ts6_manager_shared::servers::{CreateServerRequest, ServerSummary};

use crate::app_state::AppState;
use crate::auth::extractors::{RequireAdmin, RequireAuth};
use crate::crypto;
use crate::repos::server_connections::{self, NewServerConnection};
use crate::routes::server_summary_from_row;

const DEFAULT_WEBQUERY_PORT: i64 = 10080;
const DEFAULT_SSH_PORT: i64 = 10022;

/// Build the `/api/servers` sub-router. Uses an absolute path so it can be
/// `merge`d at the top-level alongside the dashboard route — `nest` was
/// avoided because axum 0.8 enforces strict trailing-slash matching, and
/// spec §7.5 names the endpoint `/api/servers` (no trailing slash).
pub fn router() -> Router<AppState> {
    Router::new().route("/api/servers", get(list).post(create))
}

async fn list(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
) -> Result<Json<Vec<ServerSummary>>, Response> {
    // Admins see every row; everyone else is filtered by the
    // `server_user_grant` join (§6.6 + §7.5).
    let rows = if user.is_admin() {
        server_connections::list(&state.db).await.map_err(|e| {
            tracing::error!(err = %e, "list_servers: list query failed");
            internal()
        })?
    } else {
        server_connections::list_for_user(&state.db, user.id)
            .await
            .map_err(|e| {
                tracing::error!(err = %e, user_id = user.id, "list_servers: grant-join failed");
                internal()
            })?
    };
    Ok(Json(
        rows.into_iter().map(server_summary_from_row).collect(),
    ))
}

async fn create(
    State(state): State<AppState>,
    RequireAdmin(_admin): RequireAdmin,
    Json(req): Json<CreateServerRequest>,
) -> Result<(StatusCode, Json<ServerSummary>), Response> {
    // Seal at rest BEFORE insert — if seal fails we never touch the DB,
    // and we never store plaintext alongside ciphertext (spec §6.3.2).
    let sealed_api_key = crypto::seal(&req.api_key).map_err(|e| {
        tracing::error!(err = %e, "create_server: apiKey seal failed");
        internal()
    })?;
    let sealed_ssh_password = match req.ssh_password.as_deref() {
        Some(s) if !s.is_empty() => Some(crypto::seal(s).map_err(|e| {
            tracing::error!(err = %e, "create_server: sshPassword seal failed");
            internal()
        })?),
        _ => None,
    };

    // D-SSH-AUTH (PURA-77): the new SSHBridge auth fields are not part of
    // the public `CreateServerRequest` body yet — they default to None here
    // and the migration's `DEFAULT 'webquery'` / `DEFAULT 'password'` clauses
    // produce the spec-equivalent row. PURA-69 follow-up C extends this
    // handler with the key/agent/fingerprint fields once SecurityEngineer
    // signs off on the wire surface.
    let new = NewServerConnection {
        name: req.name,
        host: req.host,
        webqueryPort: req.webquery_port.unwrap_or(DEFAULT_WEBQUERY_PORT),
        apiKey: sealed_api_key,
        useHttps: req.use_https.unwrap_or(false),
        sshPort: req.ssh_port.unwrap_or(DEFAULT_SSH_PORT),
        sshUsername: req.ssh_username.filter(|s| !s.is_empty()),
        sshPassword: sealed_ssh_password,
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

    let row = server_connections::insert(&state.db, new)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, "create_server: insert failed");
            internal()
        })?;
    Ok((StatusCode::CREATED, Json(server_summary_from_row(row))))
}

fn err(status: StatusCode, body: &str) -> Response {
    (status, Json(ErrorResponse::new(body))).into_response()
}

fn internal() -> Response {
    err(StatusCode::INTERNAL_SERVER_ERROR, "Internal error")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{jwt, password};
    use crate::db::{connect_in_memory, migrations};
    use crate::repos::{server_user_grants, users};
    use axum::body::Body;
    use axum::http::{HeaderValue, Method, Request};
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use std::time::Duration;
    use tower::ServiceExt;

    async fn fresh_state() -> AppState {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        crate::crypto::init("test-seed-pura-22");
        let control = crate::control::ControlBackendPool::new(false, db.clone());
        AppState {
            db,
            jwt_secret: Arc::new(b"test-secret-bytes-please-32-or-more".to_vec()),
            jwt_access_expiry: Duration::from_secs(900),
            jwt_refresh_expiry: Duration::from_secs(7 * 24 * 3600),
            setup_lock: Arc::new(tokio::sync::Mutex::new(())),
            webquery: crate::webquery::WebQueryPool::new(false),
            control,
            ws_hub: crate::ws::Hub::new(),
            widget_cache: crate::widgets::WidgetCache::new(),
            music_bots: crate::music_bots::MusicBotService::default_for_tests(),
            sidecar: None,
            ssrf_resolver: Arc::new(ts6_ssrf::MockResolver::new()),
        }
    }

    fn app(state: AppState) -> Router {
        Router::new().merge(router()).with_state(state)
    }

    fn json_body<T: serde::Serialize>(value: &T) -> Body {
        Body::from(serde_json::to_vec(value).unwrap())
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

    async fn seed_user(state: &AppState, username: &str, role: &str) -> i64 {
        let pw = "Hunter2!ok".to_string();
        let hash = tokio::task::spawn_blocking(move || password::hash_new(&pw))
            .await
            .unwrap()
            .unwrap();
        users::insert(
            &state.db,
            users::NewUser {
                username: username.into(),
                passwordHash: hash,
                displayName: username.into(),
                role: role.into(),
                enabled: true,
            },
        )
        .await
        .unwrap()
        .id
    }

    fn mint_token(state: &AppState, id: i64, username: &str, role: &str) -> String {
        jwt::mint_access(
            id,
            username,
            role,
            state.jwt_access_expiry,
            &state.jwt_secret,
        )
        .unwrap()
    }

    fn auth_header(token: &str) -> HeaderValue {
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap()
    }

    fn create_body() -> CreateServerRequest {
        CreateServerRequest {
            name: "Primary".into(),
            host: "ts.example.com".into(),
            webquery_port: Some(10080),
            api_key: "WEBQUERY-KEY-PLAINTEXT".into(),
            use_https: Some(true),
            ssh_port: Some(10022),
            ssh_username: Some("serveradmin".into()),
            ssh_password: Some("ssh-secret-pw".into()),
        }
    }

    #[tokio::test]
    async fn admin_can_create_server_and_response_omits_secrets() {
        let state = fresh_state().await;
        let aid = seed_user(&state, "admin", "admin").await;
        let token = mint_token(&state, aid, "admin", "admin");
        let app = app(state.clone());

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/servers")
                    .header("authorization", auth_header(&token))
                    .header("content-type", "application/json")
                    .body(json_body(&create_body()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        // Pull the response BODY into raw text so we can pin the spec
        // §7.5 invariant ("apiKey MUST NOT appear in any response") at
        // the wire level, not just the typed level. The trailing checks
        // also pin the D-SSH-AUTH (PURA-77) deviation gate: the new
        // SSHBridge auth fields MUST stay off `/api/servers` until
        // SecurityEngineer signs off on the public surface.
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let raw = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(!raw.contains("apiKey"), "apiKey leaked: {raw}");
        assert!(!raw.contains("sshPassword"), "sshPassword leaked: {raw}");
        for forbidden in [
            "controlPath",
            "sshAuthMethod",
            "sshPrivateKey",
            "sshKeyAgentSocket",
            "sshHostKeyFingerprint",
        ] {
            assert!(
                !raw.contains(forbidden),
                "D-SSH-AUTH field `{forbidden}` leaked to /api/servers: {raw}"
            );
        }
        let body: ServerSummary = serde_json::from_str(&raw).unwrap();
        assert_eq!(body.name, "Primary");
        assert!(body.has_ssh_credentials);

        // DB-side: apiKey + sshPassword are sealed.
        let rows = server_connections::list(&state.db).await.unwrap();
        assert!(rows[0].apiKey.starts_with("enc:"));
        assert_eq!(
            crate::crypto::unseal(&rows[0].apiKey).unwrap(),
            "WEBQUERY-KEY-PLAINTEXT"
        );
    }

    #[tokio::test]
    async fn non_admin_cannot_create_server() {
        let state = fresh_state().await;
        let mid = seed_user(&state, "modr", "moderator").await;
        let vid = seed_user(&state, "view", "viewer").await;
        let app = app(state.clone());

        for (id, name, role) in [(mid, "modr", "moderator"), (vid, "view", "viewer")] {
            let token = mint_token(&state, id, name, role);
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/api/servers")
                        .header("authorization", auth_header(&token))
                        .header("content-type", "application/json")
                        .body(json_body(&create_body()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::FORBIDDEN,
                "role={role} must not be allowed to POST /api/servers"
            );
        }

        // No row was written — RBAC kicked in before the seal+insert path.
        let rows = server_connections::list(&state.db).await.unwrap();
        assert!(
            rows.is_empty(),
            "moderator/viewer must not be able to create server rows"
        );
    }

    #[tokio::test]
    async fn list_requires_authentication() {
        let state = fresh_state().await;
        let app = app(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/servers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn list_for_admin_returns_every_row() {
        let state = fresh_state().await;
        let aid = seed_user(&state, "admin", "admin").await;
        let token = mint_token(&state, aid, "admin", "admin");
        let app = app(state.clone());

        // Seed two server rows directly via the repo (no grants on either).
        for n in 1..=2 {
            server_connections::insert(
                &state.db,
                NewServerConnection {
                    name: format!("Server{n}"),
                    host: "ts.example.com".into(),
                    webqueryPort: 10080,
                    apiKey: crate::crypto::seal("k").unwrap(),
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
            .unwrap();
        }

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/servers")
                    .header("authorization", auth_header(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Vec<ServerSummary> = read_json(resp).await;
        assert_eq!(body.len(), 2, "admin must see every server");
    }

    /// Per-grant visibility for non-admins. Spec §7.5 + §6.6: a viewer
    /// only sees the servers they have a `server_user_grant` for.
    #[tokio::test]
    async fn list_for_non_admin_filters_by_grant_join() {
        let state = fresh_state().await;
        let vid = seed_user(&state, "view", "viewer").await;
        let token = mint_token(&state, vid, "view", "viewer");
        let app = app(state.clone());

        // Three servers; the viewer is granted access to the second only.
        let mut server_ids = Vec::new();
        for n in 1..=3 {
            let row = server_connections::insert(
                &state.db,
                NewServerConnection {
                    name: format!("Server{n}"),
                    host: "ts.example.com".into(),
                    webqueryPort: 10080,
                    apiKey: crate::crypto::seal("k").unwrap(),
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
            .unwrap();
            server_ids.push(row.id);
        }
        server_user_grants::insert(&state.db, vid, server_ids[1])
            .await
            .unwrap();

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/servers")
                    .header("authorization", auth_header(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Vec<ServerSummary> = read_json(resp).await;
        assert_eq!(body.len(), 1, "viewer must see only granted servers");
        assert_eq!(body[0].id, server_ids[1]);
        assert_eq!(body[0].name, "Server2");
    }

    /// Non-admins with NO grants must see an empty list — never the
    /// admin's view by default.
    #[tokio::test]
    async fn list_for_non_admin_with_no_grants_returns_empty() {
        let state = fresh_state().await;
        let vid = seed_user(&state, "view", "viewer").await;
        let token = mint_token(&state, vid, "view", "viewer");
        let app = app(state.clone());

        for n in 1..=2 {
            server_connections::insert(
                &state.db,
                NewServerConnection {
                    name: format!("Server{n}"),
                    host: "ts.example.com".into(),
                    webqueryPort: 10080,
                    apiKey: crate::crypto::seal("k").unwrap(),
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
            .unwrap();
        }

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/servers")
                    .header("authorization", auth_header(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: Vec<ServerSummary> = read_json(resp).await;
        assert!(body.is_empty(), "viewer with no grants must see empty list");
    }
}
