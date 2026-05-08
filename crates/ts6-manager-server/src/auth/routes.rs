//! Spec §7.1 — `/api/auth/*` REST handlers.
//!
//! Wires Phase 1 SECURITY's primitives (`auth::password`, `auth::jwt`,
//! `auth::refresh`, `auth::complexity`) into the five auth routes the SPA
//! needs: login / refresh / logout / me / password.
//!
//! Per-route notes:
//! - `POST /api/auth/login` — verify password, mint access JWT, issue first
//!   refresh token in a fresh family. Per-IP rate limit (15 reqs / 15 min,
//!   spec §6.8) is layered on by the caller of [`router`] via
//!   [`crate::web::rate_limit`].
//! - `POST /api/auth/refresh` — rotate via `auth::refresh::rotate`. Reuse
//!   detection (R5) is enforced inside that function; this handler just
//!   maps `InvalidOrExpired` → 401 with the spec body. Shares the auth
//!   rate-limit bucket with `/login` (single attacker can't side-step the
//!   budget by alternating endpoints).
//! - `POST /api/auth/logout` — delete the refresh token. No auth required;
//!   the refresh token IS the credential. 204 regardless of whether a row
//!   was deleted (idempotent per spec §6.5.5).
//! - `GET /api/auth/me` — return the current user's [`UserInfo`].
//! - `PUT /api/auth/password` — verify current, validate new, re-hash, then
//!   revoke every refresh token for the user (spec §6.2.3). 204.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::middleware::from_fn_with_state;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use ts6_manager_shared::auth::{
    ChangePasswordRequest, ErrorResponse, LoginRequest, LogoutRequest, RefreshRequest,
    TokenPairResponse, UserInfo, auth_error_strings as msg,
};

use crate::app_state::AppState;
use crate::auth::extractors::{AuthUser, RequireAuth};
use crate::auth::{complexity, jwt, password, refresh};
use crate::repos::{refresh_tokens, users};
use crate::web::rate_limit::{RateLimitState, rate_limit_auth};

/// Build the `/api/auth` sub-router. The caller nests it under `/api/auth`
/// so the route paths in this module stay short.
///
/// `/login` and `/refresh` are wrapped in the spec §6.8 per-IP rate-limit
/// middleware via the caller-supplied [`RateLimitState`]. `/logout`,
/// `/me`, and `/password` are unrestricted (they are either credential-
/// less or already JWT-gated).
pub fn router(rate_limit: RateLimitState) -> Router<AppState> {
    let rl_layer = from_fn_with_state(rate_limit, rate_limit_auth);
    Router::new()
        .route("/login", post(login).layer(rl_layer.clone()))
        .route("/refresh", post(refresh_handler).layer(rl_layer))
        .route("/logout", post(logout))
        .route("/me", get(me))
        .route("/password", put(change_password))
}

/// Build the absolute WS routes as a `Router<AppState>` so they can be
/// merged into the top-level router with state baked in alongside the auth
/// routes. Phase 1 SECURITY (slice 4a) shipped the spec-canonical `/ws`
/// upgrade (§8.1); PURA-70 (Phase 2) also exposes `/api/ws` so the path
/// matches the Phase 2 task wording. Both paths route to the same
/// handler — see [`crate::auth::ws_handshake`].
pub fn ws_router() -> Router<AppState> {
    Router::new()
        .route("/ws", get(crate::auth::ws_handshake::ws_upgrade))
        .route("/api/ws", get(crate::auth::ws_handshake::ws_upgrade))
}

/// Convenience: spec error body + status into an `axum::Response`.
fn err(status: StatusCode, body: &str) -> Response {
    (status, Json(ErrorResponse::new(body))).into_response()
}

async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<TokenPairResponse>, Response> {
    // Constant-ish error path: same body for "no such user" and "wrong
    // password" so we don't leak which usernames exist.
    let invalid = || err(StatusCode::UNAUTHORIZED, "Invalid credentials");

    let user = users::find_by_username(&state.db, &req.username)
        .await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"))?
        .ok_or_else(invalid)?;
    if !user.enabled {
        return Err(invalid());
    }

    let stored = user.passwordHash.clone();
    let supplied = req.password.clone();
    let ok = tokio::task::spawn_blocking(move || password::verify(&stored, &supplied))
        .await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"))?
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"))?;
    if !ok {
        return Err(invalid());
    }

    let access = jwt::mint_access(
        user.id,
        &user.username,
        &user.role,
        state.jwt_access_expiry,
        &state.jwt_secret,
    )
    .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"))?;

    let issued = refresh::issue_for_login(
        &state.db,
        user.id,
        chrono::Duration::from_std(state.jwt_refresh_expiry)
            .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"))?,
    )
    .await
    .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"))?;

    // lastLoginAt bump — best-effort; failure here doesn't break login.
    let _ = users::mark_login(&state.db, user.id).await;

    Ok(Json(TokenPairResponse {
        access_token: access,
        refresh_token: issued.token,
    }))
}

async fn refresh_handler(
    State(state): State<AppState>,
    Json(req): Json<RefreshRequest>,
) -> Result<Json<TokenPairResponse>, Response> {
    let lifetime = chrono::Duration::from_std(state.jwt_refresh_expiry)
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"))?;

    let rotated = refresh::rotate(&state.db, &req.refresh_token, lifetime)
        .await
        .map_err(|_| err(StatusCode::UNAUTHORIZED, msg::INVALID_TOKEN))?;

    // The DB lookup gives us the user's CURRENT role for the new access
    // token (spec §6.5.3 step 7).
    let user = users::find_by_id(&state.db, rotated.user_id)
        .await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"))?
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, msg::USER_DISABLED))?;
    if !user.enabled {
        return Err(err(StatusCode::UNAUTHORIZED, msg::USER_DISABLED));
    }

    let access = jwt::mint_access(
        user.id,
        &user.username,
        &user.role,
        state.jwt_access_expiry,
        &state.jwt_secret,
    )
    .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"))?;

    Ok(Json(TokenPairResponse {
        access_token: access,
        refresh_token: rotated.token,
    }))
}

async fn logout(State(state): State<AppState>, Json(req): Json<LogoutRequest>) -> StatusCode {
    // Spec §6.5.5: idempotent; 204 whether or not a row existed.
    let _ = refresh_tokens::delete_by_token(&state.db, &req.refresh_token).await;
    StatusCode::NO_CONTENT
}

async fn me(RequireAuth(user): RequireAuth) -> Json<UserInfo> {
    Json(UserInfo {
        id: user.id,
        username: user.username,
        display_name: user.display_name,
        role: user.role,
    })
}

async fn change_password(
    State(state): State<AppState>,
    RequireAuth(AuthUser { id, .. }): RequireAuth,
    Json(req): Json<ChangePasswordRequest>,
) -> Result<StatusCode, Response> {
    // Re-fetch the user so we have the live passwordHash.
    let user = users::find_by_id(&state.db, id)
        .await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"))?
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, msg::USER_DISABLED))?;

    let stored = user.passwordHash.clone();
    let current = req.current_password.clone();
    let ok = tokio::task::spawn_blocking(move || password::verify(&stored, &current))
        .await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"))?
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"))?;
    if !ok {
        return Err(err(
            StatusCode::UNAUTHORIZED,
            "Current password is incorrect",
        ));
    }

    if let Err(rule) = complexity::validate(&req.new_password) {
        return Err(err(StatusCode::BAD_REQUEST, rule.message()));
    }

    let new_pw = req.new_password.clone();
    let new_hash = tokio::task::spawn_blocking(move || password::hash_new(&new_pw))
        .await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"))?
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"))?;

    users::set_password_hash(&state.db, id, new_hash)
        .await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"))?;

    // Spec §6.2.3 step 2: revoke every refresh token for this user — forces
    // re-login on every other session.
    let _ = refresh_tokens::delete_all_for_user(&state.db, id).await;

    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{connect_in_memory, migrations};
    use axum::body::Body;
    use axum::http::{HeaderValue, Method, Request};
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use std::time::Duration;
    use tower::ServiceExt;

    async fn fresh_state() -> AppState {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        AppState {
            db,
            jwt_secret: Arc::new(b"test-secret-bytes-please-32-or-more-bytes".to_vec()),
            jwt_access_expiry: Duration::from_secs(900),
            jwt_refresh_expiry: Duration::from_secs(7 * 24 * 3600),
            setup_lock: Arc::new(tokio::sync::Mutex::new(())),
            webquery: crate::webquery::WebQueryPool::new(false),
            ws_hub: crate::ws::Hub::new(),
        }
    }

    async fn seed_user(state: &AppState, username: &str, password: &str, role: &str) -> i64 {
        let pw = password.to_string();
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

    fn fresh_rate_limit() -> RateLimitState {
        RateLimitState {
            limiter: crate::web::rate_limit::make_auth_limiter(),
            trusted_hops: 0,
        }
    }

    fn app(state: AppState) -> Router {
        Router::new()
            .nest("/api/auth", router(fresh_rate_limit()))
            .with_state(state)
    }

    #[tokio::test]
    async fn login_happy_path_returns_token_pair() {
        let state = fresh_state().await;
        seed_user(&state, "alice", "Hunter2!ok", "viewer").await;

        let resp = app(state)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/auth/login")
                    .header("content-type", "application/json")
                    .body(json_body(&LoginRequest {
                        username: "alice".into(),
                        password: "Hunter2!ok".into(),
                    }))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: TokenPairResponse = read_json(resp).await;
        assert!(!body.access_token.is_empty());
        assert_eq!(body.refresh_token.len(), 128, "spec §6.5.1: 64 bytes hex");
    }

    #[tokio::test]
    async fn login_wrong_password_returns_401() {
        let state = fresh_state().await;
        seed_user(&state, "alice", "Hunter2!ok", "viewer").await;
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/auth/login")
                    .header("content-type", "application/json")
                    .body(json_body(&LoginRequest {
                        username: "alice".into(),
                        password: "wrong".into(),
                    }))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn login_unknown_user_returns_401() {
        let state = fresh_state().await;
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/auth/login")
                    .header("content-type", "application/json")
                    .body(json_body(&LoginRequest {
                        username: "nobody".into(),
                        password: "Hunter2!ok".into(),
                    }))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn refresh_rotates_and_returns_new_pair() {
        let state = fresh_state().await;
        seed_user(&state, "alice", "Hunter2!ok", "viewer").await;
        let app = app(state);

        // Login.
        let login_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/auth/login")
                    .header("content-type", "application/json")
                    .body(json_body(&LoginRequest {
                        username: "alice".into(),
                        password: "Hunter2!ok".into(),
                    }))
                    .unwrap(),
            )
            .await
            .unwrap();
        let pair1: TokenPairResponse = read_json(login_resp).await;

        // Refresh.
        let refresh_resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/auth/refresh")
                    .header("content-type", "application/json")
                    .body(json_body(&RefreshRequest {
                        refresh_token: pair1.refresh_token.clone(),
                    }))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(refresh_resp.status(), StatusCode::OK);
        let pair2: TokenPairResponse = read_json(refresh_resp).await;
        // Refresh-token rotation is the security-critical invariant; access
        // tokens minted within the same second are byte-identical (same id,
        // username, role, iat, exp under the same secret) and that is fine.
        assert_ne!(pair1.refresh_token, pair2.refresh_token);
        assert!(!pair2.access_token.is_empty());
    }

    #[tokio::test]
    async fn me_with_valid_token_returns_user_info() {
        let state = fresh_state().await;
        seed_user(&state, "alice", "Hunter2!ok", "admin").await;
        let app = app(state);

        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/auth/login")
                    .header("content-type", "application/json")
                    .body(json_body(&LoginRequest {
                        username: "alice".into(),
                        password: "Hunter2!ok".into(),
                    }))
                    .unwrap(),
            )
            .await
            .unwrap();
        let pair: TokenPairResponse = read_json(login).await;

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/auth/me")
                    .header(
                        "authorization",
                        HeaderValue::from_str(&format!("Bearer {}", pair.access_token)).unwrap(),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let info: UserInfo = read_json(resp).await;
        assert_eq!(info.username, "alice");
        assert_eq!(info.role, "admin");
    }

    #[tokio::test]
    async fn me_without_token_returns_401_with_spec_body() {
        let state = fresh_state().await;
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .uri("/api/auth/me")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body: ErrorResponse = read_json(resp).await;
        assert_eq!(body.error, msg::NO_TOKEN);
    }

    #[tokio::test]
    async fn me_with_invalid_token_returns_401() {
        let state = fresh_state().await;
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .uri("/api/auth/me")
                    .header("authorization", "Bearer not-a-jwt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body: ErrorResponse = read_json(resp).await;
        assert_eq!(body.error, msg::INVALID_TOKEN);
    }

    #[tokio::test]
    async fn logout_returns_204_and_invalidates_refresh() {
        let state = fresh_state().await;
        seed_user(&state, "alice", "Hunter2!ok", "viewer").await;
        let app = app(state);

        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/auth/login")
                    .header("content-type", "application/json")
                    .body(json_body(&LoginRequest {
                        username: "alice".into(),
                        password: "Hunter2!ok".into(),
                    }))
                    .unwrap(),
            )
            .await
            .unwrap();
        let pair: TokenPairResponse = read_json(login).await;

        let logout = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/auth/logout")
                    .header("content-type", "application/json")
                    .body(json_body(&LogoutRequest {
                        refresh_token: pair.refresh_token.clone(),
                    }))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(logout.status(), StatusCode::NO_CONTENT);

        // Refreshing the now-deleted token must 401.
        let refreshed = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/auth/refresh")
                    .header("content-type", "application/json")
                    .body(json_body(&RefreshRequest {
                        refresh_token: pair.refresh_token,
                    }))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(refreshed.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn logout_unknown_token_still_returns_204() {
        let state = fresh_state().await;
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/auth/logout")
                    .header("content-type", "application/json")
                    .body(json_body(&LogoutRequest {
                        refresh_token: "0000".into(),
                    }))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn change_password_revokes_existing_refresh_tokens() {
        let state = fresh_state().await;
        let uid = seed_user(&state, "alice", "Hunter2!ok", "viewer").await;
        let app = app(state.clone());

        // Login → gets token A.
        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/auth/login")
                    .header("content-type", "application/json")
                    .body(json_body(&LoginRequest {
                        username: "alice".into(),
                        password: "Hunter2!ok".into(),
                    }))
                    .unwrap(),
            )
            .await
            .unwrap();
        let pair: TokenPairResponse = read_json(login).await;

        // Change password.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri("/api/auth/password")
                    .header(
                        "authorization",
                        HeaderValue::from_str(&format!("Bearer {}", pair.access_token)).unwrap(),
                    )
                    .header("content-type", "application/json")
                    .body(json_body(&ChangePasswordRequest {
                        current_password: "Hunter2!ok".into(),
                        new_password: "NewPassw0rd!".into(),
                    }))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // The old refresh token must no longer rotate (§6.2.3 revocation).
        let leftover = refresh_tokens::list_for_user(&state.db, uid).await.unwrap();
        assert!(
            leftover.is_empty(),
            "all refresh tokens for user must be revoked after password change"
        );
    }

    #[tokio::test]
    async fn change_password_rejects_weak_new_password_with_spec_message() {
        let state = fresh_state().await;
        seed_user(&state, "alice", "Hunter2!ok", "viewer").await;
        let app = app(state);

        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/auth/login")
                    .header("content-type", "application/json")
                    .body(json_body(&LoginRequest {
                        username: "alice".into(),
                        password: "Hunter2!ok".into(),
                    }))
                    .unwrap(),
            )
            .await
            .unwrap();
        let pair: TokenPairResponse = read_json(login).await;

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri("/api/auth/password")
                    .header(
                        "authorization",
                        HeaderValue::from_str(&format!("Bearer {}", pair.access_token)).unwrap(),
                    )
                    .header("content-type", "application/json")
                    .body(json_body(&ChangePasswordRequest {
                        current_password: "Hunter2!ok".into(),
                        new_password: "abc".into(), // too short, no upper, no digit, no special
                    }))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// Spec §6.13 verify test: 16 login attempts in tight succession must
    /// see the 16th rejected with HTTP 429. This exercises the full
    /// `/api/auth/login` path through the rate-limit middleware to confirm
    /// the wiring in [`router`] is correct (not just the standalone
    /// middleware unit tests in [`crate::web::rate_limit`]).
    #[tokio::test]
    async fn login_429_after_15_attempt_burst_with_spec_body_and_retry_after() {
        let state = fresh_state().await;
        // Wrong password keeps the bucket-burn loop fast (no Argon2 cycles
        // on the success path) and the test independent of any seeded user.
        let app = app(state);
        let body = || {
            json_body(&LoginRequest {
                username: "nobody".into(),
                password: "wrong".into(),
            })
        };

        // First 15 attempts must NOT see 429 — they may be 401 or 200, the
        // rate-limit decision is what we're pinning here, not credential
        // validity.
        for n in 1..=15 {
            let mut req = Request::builder()
                .method(Method::POST)
                .uri("/api/auth/login")
                .header("content-type", "application/json")
                .body(body())
                .unwrap();
            req.extensions_mut().insert(axum::extract::ConnectInfo(
                std::net::SocketAddr::from(([198, 51, 100, 5], 50_000)),
            ));
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_ne!(
                resp.status(),
                StatusCode::TOO_MANY_REQUESTS,
                "attempt {n} hit the rate limit before the 15-burst window was exhausted"
            );
        }

        // 16th attempt — must be 429 with the exact spec body and a
        // Retry-After header.
        let mut req = Request::builder()
            .method(Method::POST)
            .uri("/api/auth/login")
            .header("content-type", "application/json")
            .body(body())
            .unwrap();
        req.extensions_mut().insert(axum::extract::ConnectInfo(
            std::net::SocketAddr::from(([198, 51, 100, 5], 50_000)),
        ));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(
            resp.headers().contains_key("retry-after"),
            "spec §6.8 mandates a Retry-After header on 429"
        );
        let err: ErrorResponse = read_json(resp).await;
        assert_eq!(err.error, msg::RATE_LIMIT_AUTH);
    }
}
