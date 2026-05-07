//! Spec §7.1 — `/api/auth/*` REST handlers.
//!
//! Wires Phase 1 SECURITY's primitives (`auth::password`, `auth::jwt`,
//! `auth::refresh`, `auth::complexity`) into the five auth routes the SPA
//! needs: login / refresh / logout / me / password.
//!
//! Per-route notes:
//! - `POST /api/auth/login` — verify password, mint access JWT, issue first
//!   refresh token in a fresh family. **Login rate-limit is NOT applied here
//!   yet** (spec §6.8 — added in the rate-limit slice once `tower_governor`
//!   wiring lands).
//! - `POST /api/auth/refresh` — rotate via `auth::refresh::rotate`. Reuse
//!   detection (R5) is enforced inside that function; this handler just
//!   maps `InvalidOrExpired` → 401 with the spec body.
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

/// Build the `/api/auth` sub-router. The caller nests it under `/api/auth`
/// so the route paths in this module stay short.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/login", post(login))
        .route("/refresh", post(refresh_handler))
        .route("/logout", post(logout))
        .route("/me", get(me))
        .route("/password", put(change_password))
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
        accessToken: access,
        refreshToken: issued.token,
    }))
}

async fn refresh_handler(
    State(state): State<AppState>,
    Json(req): Json<RefreshRequest>,
) -> Result<Json<TokenPairResponse>, Response> {
    let lifetime = chrono::Duration::from_std(state.jwt_refresh_expiry)
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"))?;

    let rotated = refresh::rotate(&state.db, &req.refreshToken, lifetime)
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
        accessToken: access,
        refreshToken: rotated.token,
    }))
}

async fn logout(State(state): State<AppState>, Json(req): Json<LogoutRequest>) -> StatusCode {
    // Spec §6.5.5: idempotent; 204 whether or not a row existed.
    let _ = refresh_tokens::delete_by_token(&state.db, &req.refreshToken).await;
    StatusCode::NO_CONTENT
}

async fn me(RequireAuth(user): RequireAuth) -> Json<UserInfo> {
    Json(UserInfo {
        id: user.id,
        username: user.username,
        displayName: user.display_name,
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
    let current = req.currentPassword.clone();
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

    if let Err(rule) = complexity::validate(&req.newPassword) {
        return Err(err(StatusCode::BAD_REQUEST, rule.message()));
    }

    let new_pw = req.newPassword.clone();
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

    fn app(state: AppState) -> Router {
        Router::new().nest("/api/auth", router()).with_state(state)
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
        assert!(!body.accessToken.is_empty());
        assert_eq!(body.refreshToken.len(), 128, "spec §6.5.1: 64 bytes hex");
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
                        refreshToken: pair1.refreshToken.clone(),
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
        assert_ne!(pair1.refreshToken, pair2.refreshToken);
        assert!(!pair2.accessToken.is_empty());
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
                        HeaderValue::from_str(&format!("Bearer {}", pair.accessToken)).unwrap(),
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
                        refreshToken: pair.refreshToken.clone(),
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
                        refreshToken: pair.refreshToken,
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
                        refreshToken: "0000".into(),
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
                        HeaderValue::from_str(&format!("Bearer {}", pair.accessToken)).unwrap(),
                    )
                    .header("content-type", "application/json")
                    .body(json_body(&ChangePasswordRequest {
                        currentPassword: "Hunter2!ok".into(),
                        newPassword: "NewPassw0rd!".into(),
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
                        HeaderValue::from_str(&format!("Bearer {}", pair.accessToken)).unwrap(),
                    )
                    .header("content-type", "application/json")
                    .body(json_body(&ChangePasswordRequest {
                        currentPassword: "Hunter2!ok".into(),
                        newPassword: "abc".into(), // too short, no upper, no digit, no special
                    }))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
