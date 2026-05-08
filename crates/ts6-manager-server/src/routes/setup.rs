//! Spec §7.2 — `/api/setup/{status,init}`. PURA-22 slice 5.
//!
//! `GET /api/setup/status` (unauthenticated): returns `{ needsSetup: bool }`
//! computed from `user_count == 0`.
//!
//! `POST /api/setup/init` (unauthenticated, but only meaningful while
//! `needsSetup == true`): one-shot creation of the bootstrap admin user
//! **and** the first `server_connection` row. Concurrent inits MUST resolve
//! to one success + one `409 already_initialized` (PURA-22 acceptance) —
//! enforced by [`AppState::setup_lock`] held across the count-check + the
//! atomic insert pair (see [`crate::repos::setup::init_admin_and_first_server`]).
//!
//! Security lenses applied:
//! - **AuthN/AuthZ**: setup is intentionally credential-less; the gate is
//!   `user_count == 0`. Once any user exists, the endpoint hard-fails.
//! - **Cryptography**: the admin password goes through Argon2id
//!   ([`crate::auth::password::hash_new`]) on a blocking thread; `apiKey`
//!   and `sshPassword` are AES-256-GCM-sealed via [`crate::crypto::seal`]
//!   *before* the DB write so plaintext never touches a partially-written
//!   row (spec §6.3).
//! - **Input handling**: password complexity (§6.2.2) is checked before
//!   hashing so we don't burn Argon2 cycles on rejects.
//! - **Rate limiting**: spec §6.8 mandates 15 reqs / 15 min on
//!   `POST /api/setup/*`. PURA-35 wires a DEDICATED limiter on
//!   `POST /api/setup/init` (see [`router`]) — distinct from the
//!   `/login` + `/refresh` bucket so login spam cannot DoS the
//!   bootstrap wizard, and a stuck setup retry cannot DoS login.
//!   `GET /api/setup/status` is read-only and stays unrestricted.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::middleware::from_fn_with_state;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use ts6_manager_shared::auth::{ErrorResponse, UserInfo};
use ts6_manager_shared::setup::{
    SetupInitRequest, SetupInitResponse, SetupStatusResponse,
};

use crate::app_state::AppState;
use crate::auth::{complexity, password};
use crate::crypto;
use crate::repos::server_connections::NewServerConnection;
use crate::repos::users::NewUser;
use crate::repos::{self, users};
use crate::routes::server_summary_from_row;
use crate::web::rate_limit::{RateLimitState, rate_limit_auth};

/// Wire-string for the one-shot 409. PURA-22 fixes the body verbatim so the
/// FE can branch without parsing English copy.
const ALREADY_INITIALIZED: &str = "already_initialized";

/// Default WebQuery port when the wizard omits it (matches spec default and
/// the `server_connection.webqueryPort` SCHEMAFULL DEFAULT in 0001_baseline).
const DEFAULT_WEBQUERY_PORT: i64 = 10080;
/// Default SSH port — same rationale as above.
const DEFAULT_SSH_PORT: i64 = 10022;

/// Build the `/api/setup` sub-router. Uses absolute paths to match the
/// `merge` style adopted across non-auth routes — see [`crate::routes::servers`]
/// for the rationale (axum 0.8 strict trailing-slash + spec §7.2 path names).
///
/// PURA-35: `POST /api/setup/init` is wrapped in the spec §6.8 per-IP
/// rate-limit middleware via the caller-supplied [`RateLimitState`].
/// `GET /api/setup/status` is read-only and unrestricted — it powers
/// the wizard's needs-setup probe and rate-limiting it would just
/// degrade the first-run UX without buying any defence.
pub fn router(rate_limit: RateLimitState) -> Router<AppState> {
    let rl_layer = from_fn_with_state(rate_limit, rate_limit_auth);
    Router::new()
        .route("/api/setup/status", get(status))
        .route("/api/setup/init", post(init).layer(rl_layer))
}

async fn status(State(state): State<AppState>) -> Result<Json<SetupStatusResponse>, Response> {
    let n = users::count(&state.db).await.map_err(|e| {
        tracing::error!(err = %e, "setup_status: user count query failed");
        internal()
    })?;
    Ok(Json(SetupStatusResponse { needs_setup: n == 0 }))
}

async fn init(
    State(state): State<AppState>,
    Json(req): Json<SetupInitRequest>,
) -> Result<(StatusCode, Json<SetupInitResponse>), Response> {
    // Serialise concurrent calls. The mutex is process-scoped (Phase 1
    // deploys a single binary). Holding it across the Argon2 hash
    // (~100ms) is acceptable: the endpoint is one-shot, run at most once
    // per deployment, and the alternative (release-then-reacquire) reopens
    // a TOCTOU window between count-check and CREATE.
    let _guard: tokio::sync::MutexGuard<'_, ()> = state.setup_lock.lock().await;

    // Defence in depth — re-check user count under the lock so a benign
    // racy /status response can't trick us into a second init.
    let n = users::count(&state.db).await.map_err(|e| {
        tracing::error!(err = %e, "setup_init: user count query failed");
        internal()
    })?;
    if n > 0 {
        return Err(err(StatusCode::CONFLICT, ALREADY_INITIALIZED));
    }

    // Validate complexity BEFORE hashing — spec §6.2.2 rejects on the
    // first violation; hashing a non-compliant password wastes ~100ms of
    // CPU per request.
    if let Err(rule) = complexity::validate(&req.password) {
        return Err(err(StatusCode::BAD_REQUEST, rule.message()));
    }

    // Argon2id hash off the runtime (blocking, CPU-bound).
    let pw = req.password.clone();
    let password_hash = tokio::task::spawn_blocking(move || password::hash_new(&pw))
        .await
        .map_err(|e| {
            tracing::error!(err = %e, "setup_init: argon2 task panicked");
            internal()
        })?
        .map_err(|e| {
            tracing::error!(err = %e, "setup_init: argon2 hash failed");
            internal()
        })?;

    // Seal credentials at rest BEFORE the DB writes: if seal fails we
    // bail before touching either table, and we never hold a row that
    // mixes plaintext + ciphertext.
    let sealed_api_key = crypto::seal(&req.server.api_key).map_err(|e| {
        tracing::error!(err = %e, "setup_init: apiKey seal failed");
        internal()
    })?;
    let sealed_ssh_password = match req.server.ssh_password.as_deref() {
        Some(s) if !s.is_empty() => Some(crypto::seal(s).map_err(|e| {
            tracing::error!(err = %e, "setup_init: sshPassword seal failed");
            internal()
        })?),
        _ => None,
    };

    let display_name = req
        .display_name
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| req.username.clone());

    let new_user = NewUser {
        username: req.username.clone(),
        passwordHash: password_hash,
        displayName: display_name,
        // Spec §7.2: "Creates the very first user as `admin`".
        role: "admin".into(),
        enabled: true,
    };
    let new_server = NewServerConnection {
        name: req.server.name,
        host: req.server.host,
        webqueryPort: req.server.webquery_port.unwrap_or(DEFAULT_WEBQUERY_PORT),
        apiKey: sealed_api_key,
        useHttps: req.server.use_https.unwrap_or(false),
        sshPort: req.server.ssh_port.unwrap_or(DEFAULT_SSH_PORT),
        sshUsername: req
            .server
            .ssh_username
            .clone()
            .filter(|s| !s.is_empty()),
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

    let (user_row, server_row) =
        repos::setup::init_admin_and_first_server(&state.db, new_user, new_server)
            .await
            .map_err(|e| {
                tracing::error!(err = ?e, "setup_init: atomic insert failed");
                internal()
            })?;

    let body = SetupInitResponse {
        user: UserInfo {
            id: user_row.id,
            username: user_row.username,
            display_name: user_row.displayName,
            role: user_row.role,
        },
        server: server_summary_from_row(server_row),
    };
    Ok((StatusCode::CREATED, Json(body)))
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
    use crate::db::{connect_in_memory, migrations};
    use crate::repos::server_connections;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use http_body_util::BodyExt;
    use std::time::Duration;
    use tower::ServiceExt;
    use ts6_manager_shared::setup::SetupInitServer;

    async fn fresh_state() -> AppState {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        // Init the process-wide AEAD with a deterministic seed for tests.
        // `crate::crypto::init` is idempotent — first writer wins, repeated
        // calls are no-ops.
        crate::crypto::init("test-seed-pura-22");
        let control = crate::control::ControlBackendPool::new(false, db.clone());
        AppState {
            db,
            jwt_secret: std::sync::Arc::new(b"test-secret-bytes-please-32-or-more".to_vec()),
            jwt_access_expiry: Duration::from_secs(900),
            jwt_refresh_expiry: Duration::from_secs(7 * 24 * 3600),
            setup_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            webquery: crate::webquery::WebQueryPool::new(false),
            control,
            ws_hub: crate::ws::Hub::new(),
            widget_cache: crate::widgets::WidgetCache::new(),
        }
    }

    fn fresh_rate_limit() -> RateLimitState {
        RateLimitState {
            limiter: crate::web::rate_limit::make_setup_limiter(),
            trusted_hops: 0,
        }
    }

    fn app(state: AppState) -> Router {
        Router::new()
            .merge(router(fresh_rate_limit()))
            .with_state(state)
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

    fn valid_init_body() -> SetupInitRequest {
        SetupInitRequest {
            username: "admin".into(),
            password: "Hunter2!ok".into(),
            display_name: Some("Admin".into()),
            server: SetupInitServer {
                name: "Primary".into(),
                host: "ts.example.com".into(),
                webquery_port: Some(10080),
                api_key: "WEBQUERY-KEY-PLAINTEXT".into(),
                use_https: Some(true),
                ssh_port: Some(10022),
                ssh_username: Some("serveradmin".into()),
                ssh_password: Some("ssh-secret-pw".into()),
            },
        }
    }

    #[tokio::test]
    async fn status_starts_at_needs_setup_true() {
        let state = fresh_state().await;
        let app = app(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/setup/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: SetupStatusResponse = read_json(resp).await;
        assert!(body.needs_setup);
    }

    #[tokio::test]
    async fn happy_path_creates_admin_and_server_then_status_flips() {
        let state = fresh_state().await;
        let app = app(state.clone());

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/setup/init")
                    .header("content-type", "application/json")
                    .body(json_body(&valid_init_body()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body: SetupInitResponse = read_json(resp).await;
        assert_eq!(body.user.username, "admin");
        // Spec §7.2 — "Creates the very first user as `admin`".
        assert_eq!(body.user.role, "admin");
        assert_eq!(body.server.name, "Primary");
        // Spec §7.5 — `apiKey` MUST NOT appear in any response. The wire
        // type omits the field by construction; this assertion pins the
        // contract at the route layer too.
        let raw = serde_json::to_string(&body).unwrap();
        assert!(!raw.contains("apiKey"));
        assert!(!raw.contains("sshPassword"));

        // status now reports needsSetup=false.
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/setup/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status_body: SetupStatusResponse = read_json(resp).await;
        assert!(!status_body.needs_setup);
    }

    #[tokio::test]
    async fn second_init_returns_409_already_initialized() {
        let state = fresh_state().await;
        let app = app(state);

        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/setup/init")
                    .header("content-type", "application/json")
                    .body(json_body(&valid_init_body()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::CREATED);

        // Different username this time — proves the gate is "any user
        // exists", not "this username exists".
        let mut second_body = valid_init_body();
        second_body.username = "operator".into();
        second_body.server.name = "Secondary".into();
        let second = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/setup/init")
                    .header("content-type", "application/json")
                    .body(json_body(&second_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::CONFLICT);
        let err: ErrorResponse = read_json(second).await;
        assert_eq!(err.error, ALREADY_INITIALIZED);
    }

    /// Sealed-at-rest assertion. The DB row's `apiKey` and `sshPassword`
    /// columns MUST be ciphertext (`enc:...`) — never the plaintext that
    /// the wizard supplied. Spec §6.3.
    #[tokio::test]
    async fn server_credentials_are_sealed_at_rest() {
        let state = fresh_state().await;
        let app = app(state.clone());

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/setup/init")
                    .header("content-type", "application/json")
                    .body(json_body(&valid_init_body()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let rows = server_connections::list(&state.db).await.unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];

        // apiKey is ciphertext, prefixed `enc:` per spec §6.3.2.
        assert!(
            row.apiKey.starts_with("enc:"),
            "apiKey must be sealed at rest, got {:?}",
            row.apiKey
        );
        assert_ne!(
            row.apiKey, "WEBQUERY-KEY-PLAINTEXT",
            "plaintext apiKey leaked into the DB row"
        );
        // Round-trip — unseal returns the original plaintext.
        let recovered = crate::crypto::unseal(&row.apiKey).unwrap();
        assert_eq!(recovered, "WEBQUERY-KEY-PLAINTEXT");

        // sshPassword: same contract.
        let stored_ssh = row.sshPassword.as_deref().expect("sshPassword set");
        assert!(stored_ssh.starts_with("enc:"));
        assert_ne!(stored_ssh, "ssh-secret-pw");
        assert_eq!(crate::crypto::unseal(stored_ssh).unwrap(), "ssh-secret-pw");
    }

    /// Concurrent inits resolve to one success + one 409. PURA-22
    /// acceptance criterion. The mutex serialises the handlers; once the
    /// first commits, the second sees user_count > 0 and 409s.
    #[tokio::test]
    async fn concurrent_init_yields_exactly_one_success_and_one_conflict() {
        let state = fresh_state().await;
        let app = app(state);

        let mut a_body = valid_init_body();
        a_body.username = "alice".into();
        let mut b_body = valid_init_body();
        b_body.username = "bob".into();
        b_body.server.name = "Secondary".into();

        let app_a = app.clone();
        let app_b = app.clone();
        let (resp_a, resp_b) = tokio::join!(
            async move {
                app_a
                    .oneshot(
                        Request::builder()
                            .method(Method::POST)
                            .uri("/api/setup/init")
                            .header("content-type", "application/json")
                            .body(json_body(&a_body))
                            .unwrap(),
                    )
                    .await
                    .unwrap()
            },
            async move {
                app_b
                    .oneshot(
                        Request::builder()
                            .method(Method::POST)
                            .uri("/api/setup/init")
                            .header("content-type", "application/json")
                            .body(json_body(&b_body))
                            .unwrap(),
                    )
                    .await
                    .unwrap()
            }
        );

        let mut statuses = [resp_a.status(), resp_b.status()];
        statuses.sort_by_key(|s| s.as_u16());
        assert_eq!(
            statuses,
            [StatusCode::CREATED, StatusCode::CONFLICT],
            "concurrent inits must resolve to exactly one 201 and one 409"
        );
    }

    #[tokio::test]
    async fn weak_password_rejected_with_spec_message() {
        let state = fresh_state().await;
        let app = app(state);

        let mut body = valid_init_body();
        body.password = "abc".into(); // too short, no upper, no digit, no special

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/setup/init")
                    .header("content-type", "application/json")
                    .body(json_body(&body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let err: ErrorResponse = read_json(resp).await;
        assert!(
            err.error.starts_with("Password must"),
            "spec §6.2.2 mandates a per-rule message; got {:?}",
            err.error
        );
    }
}
