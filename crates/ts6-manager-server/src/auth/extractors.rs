//! Spec §6.4.1 / §6.4.2 — Axum extractors for authentication and role gating.
//!
//! [`RequireAuth`] is the canonical extractor: it parses the `Authorization:
//! Bearer <jwt>` header, verifies the JWT, looks up the user row in SurrealDB,
//! and returns an [`AuthUser`]. **The role used downstream comes from the DB
//! lookup, not the JWT claim** (spec §6.4.1) — revoking a user's role takes
//! effect immediately.
//!
//! Browser `EventSource` cannot set `Authorization` headers. Routes that
//! must stay EventSource-compatible (today: music-bot SSE) use
//! [`RequireAuthOrQueryToken`] instead: Bearer still works, and
//! `?token=<access_jwt>` authenticates via the same
//! [`crate::auth::ws_handshake::authenticate_token`] path as `/api/ws`.
//!
//! [`RequireRole`] composes on top of `RequireAuth` to gate routes by role
//! membership. [`RequireServerAccess`] is the spec §6.6 / `Y+access` gate
//! for `/api/servers/{configId}/...`: admin bypasses grants; every other
//! role needs a `server_user_grant` row. Missing `server_connection` is
//! `404`, matching [`crate::routes::control::access::check_read`].

use std::convert::Infallible;
use std::marker::PhantomData;
use std::net::SocketAddr;

use axum::Json;
use axum::extract::{ConnectInfo, FromRef, FromRequestParts, Path, Query};
use axum::http::StatusCode;
use axum::http::header::{AUTHORIZATION, USER_AGENT};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use ts6_manager_shared::auth::{ErrorResponse, auth_error_strings as msg};

use crate::app_state::AppState;
use crate::auth::jwt;
use crate::auth::permissions::{self, ModPermission};
use crate::auth::ws_handshake::{WsAuthError, WsTokenQuery, authenticate_token};
use crate::db::Database;
use crate::repos::server_connections::ServerConnection;
use crate::repos::{server_connections, server_user_grants, user_permissions, users};
use crate::web::proxy;

/// User context attached to a request after [`RequireAuth`] succeeds.
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    /// **Database-current role**, not the JWT's claim. See §6.4.1.
    pub role: String,
    pub enabled: bool,
}

impl AuthUser {
    pub fn is_admin(&self) -> bool {
        self.role == "admin"
    }
    pub fn is_at_least_moderator(&self) -> bool {
        self.role == "admin" || self.role == "moderator"
    }
}

/// Axum extractor that authenticates the request via Bearer JWT and a fresh
/// DB user lookup. Use as the first parameter on any handler that requires
/// auth.
#[derive(Debug, Clone)]
pub struct RequireAuth(pub AuthUser);

impl<S> FromRequestParts<S> for RequireAuth
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app: AppState = AppState::from_ref(state);
        let path = parts.uri.path().to_owned();

        // Spec §6.4.1 step 1: Authorization header MUST start with "Bearer ".
        let bearer = match parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
        {
            Some(b) => b,
            None => {
                // PURA-226 — failure mode #1 sub-case: bearer never reached
                // the extractor. The SPA gate treats this as session-killing,
                // so trace it with the path so the operator can correlate
                // FE `gate.401.session_killing` with the BE rejection. `debug`
                // level keeps the line out of `info` production logs by
                // default.
                tracing::debug!(path = %path, sub_code = "no_token", "auth 401");
                return Err(AuthError::NoToken);
            }
        };

        // Step 2: HS256 verify.
        let claims = match jwt::verify_access(bearer, &app.jwt_secret) {
            Ok(c) => c,
            Err(_) => {
                tracing::debug!(path = %path, sub_code = "invalid_token", "auth 401");
                return Err(AuthError::Invalid);
            }
        };

        // Step 3: DB lookup. Disabled or missing → 401 with the spec body.
        let user = match users::find_by_id(&app.db, claims.id).await {
            Ok(Some(u)) => u,
            Ok(None) => {
                tracing::debug!(
                    path = %path,
                    sub_code = "user_disabled",
                    user_id = claims.id,
                    reason = "user_row_missing",
                    "auth 401"
                );
                return Err(AuthError::Disabled);
            }
            Err(_) => {
                tracing::debug!(
                    path = %path,
                    sub_code = "invalid_token",
                    user_id = claims.id,
                    reason = "db_lookup_error",
                    "auth 401"
                );
                return Err(AuthError::Invalid);
            }
        };
        if !user.enabled {
            tracing::debug!(
                path = %path,
                sub_code = "user_disabled",
                user_id = user.id,
                reason = "user_row_disabled",
                "auth 401"
            );
            return Err(AuthError::Disabled);
        }

        Ok(RequireAuth(AuthUser {
            id: user.id,
            username: user.username,
            display_name: user.displayName,
            role: user.role,
            enabled: user.enabled,
        }))
    }
}

/// EventSource-compatible auth: `Authorization: Bearer` **or**
/// `?token=<access_jwt>`.
///
/// Bearer is tried first so existing REST clients keep working unchanged.
/// When no Bearer is present, the `token` query param is resolved with
/// [`authenticate_token`] — the same access-JWT + live DB-role path the
/// operator WebSocket handshake uses. Other `/api/*` routes stay on
/// [`RequireAuth`] (Bearer only) so access JWTs are not accepted on
/// arbitrary query strings.
#[derive(Debug, Clone)]
pub struct RequireAuthOrQueryToken(pub AuthUser);

impl<S> FromRequestParts<S> for RequireAuthOrQueryToken
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let has_bearer = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|s| s.starts_with("Bearer "));

        if has_bearer {
            let RequireAuth(user) = RequireAuth::from_request_parts(parts, state).await?;
            return Ok(Self(user));
        }

        let query_token = Query::<WsTokenQuery>::from_request_parts(parts, state)
            .await
            .ok()
            .and_then(|Query(q)| q.token)
            .filter(|t| !t.is_empty());

        let Some(token) = query_token else {
            let path = parts.uri.path().to_owned();
            tracing::debug!(path = %path, sub_code = "no_token", "auth 401");
            return Err(AuthError::NoToken);
        };

        let app: AppState = AppState::from_ref(state);
        match authenticate_token(&app, &token).await {
            Ok(user) => Ok(Self(user)),
            Err(WsAuthError::InvalidOrExpired) => {
                tracing::debug!(
                    path = %parts.uri.path(),
                    sub_code = "invalid_token",
                    "auth 401"
                );
                Err(AuthError::Invalid)
            }
            Err(WsAuthError::Disabled) => {
                tracing::debug!(
                    path = %parts.uri.path(),
                    sub_code = "user_disabled",
                    reason = "query_token_disabled_or_missing",
                    "auth 401"
                );
                Err(AuthError::Disabled)
            }
            Err(WsAuthError::Backend) => {
                tracing::debug!(
                    path = %parts.uri.path(),
                    sub_code = "invalid_token",
                    reason = "db_lookup_error",
                    "auth 401"
                );
                Err(AuthError::Invalid)
            }
        }
    }
}

/// Generic role-gating extractor. `RequireRole<{ Allowed::ADMIN }>` etc. is
/// awkward with Rust's const-generics surface for slice types, so we expose
/// concrete aliases below instead of a const-generic flag set.
#[derive(Debug, Clone)]
pub struct RequireAdmin(pub AuthUser);

impl<S> FromRequestParts<S> for RequireAdmin
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let RequireAuth(user) = RequireAuth::from_request_parts(parts, state).await?;
        if !user.is_admin() {
            return Err(AuthError::Forbidden);
        }
        Ok(RequireAdmin(user))
    }
}

/// Admin OR moderator. Used by routes that admin and mods can both write to
/// per spec §6.12 (flows, music bots, widgets — when those routes land).
#[derive(Debug, Clone)]
pub struct RequireModerator(pub AuthUser);

impl<S> FromRequestParts<S> for RequireModerator
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let RequireAuth(user) = RequireAuth::from_request_parts(parts, state).await?;
        if !user.is_at_least_moderator() {
            return Err(AuthError::Forbidden);
        }
        Ok(RequireModerator(user))
    }
}

/// PURA-235 / docs/admin/audit-shape.md §4.3 — captures request metadata
/// the audit-log writer needs (client IP per spec §6.8, raw `User-Agent`
/// header). Infallible: missing values degrade to `None` so the audit row
/// can still record what it knows.
///
/// `requestUserAgent` is truncated to 1 KiB at the persistence boundary
/// inside the repo, not here — keeps the original-length string available
/// to tracing if a future caller wants it.
#[derive(Debug, Clone, Default)]
pub struct RequestMeta {
    pub ip: Option<String>,
    pub user_agent: Option<String>,
}

impl<S> FromRequestParts<S> for RequestMeta
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app: AppState = AppState::from_ref(state);
        let connect = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0);
        let ip = connect
            .map(|addr| proxy::client_ip(&parts.headers, addr, app.trusted_proxy_hops).to_string());
        let user_agent = parts
            .headers
            .get(USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        Ok(RequestMeta { ip, user_agent })
    }
}

/// Phase 9.0 / PURA-284 — action-level permission gate layered on the
/// coarse role gate. `P` is a zero-sized [`ModPermission`] marker from
/// the `permissions` catalog; the extractor is compile-time safe — you
/// cannot parameterise it with an arbitrary string.
///
/// Resolution order:
///   1. The request must pass [`RequireAuth`] (JWT + live user row).
///   2. For `admin` users, every catalog permission is implicitly held.
///   3. For all other roles, explicit grants are fetched from
///      `user_permissions` and unioned with the role default set.
///   4. If the resolved set does not contain `P::PERMISSION`, the request
///      is rejected with 403 Forbidden.
pub struct RequirePermission<P: ModPermission>(pub AuthUser, PhantomData<P>);

impl<S, P> FromRequestParts<S> for RequirePermission<P>
where
    AppState: FromRef<S>,
    S: Send + Sync,
    P: ModPermission,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app: AppState = AppState::from_ref(state);
        let RequireAuth(user) = RequireAuth::from_request_parts(parts, state).await?;

        let grants: Vec<String> = if user.is_admin() {
            vec![]
        } else {
            user_permissions::permissions_for_user(&app.db, user.id)
                .await
                .map_err(|_| AuthError::Internal)?
        };

        if !permissions::has_permission(&user.role, &grants, P::PERMISSION) {
            return Err(AuthError::Forbidden);
        }

        Ok(RequirePermission(user, PhantomData))
    }
}

/// Path parameter used by every `/api/servers/{configId}/...` control route.
/// A missing or non-integer segment is `400` per spec §6.6.
#[derive(Debug, Deserialize)]
struct ConfigIdParam {
    #[serde(rename = "configId")]
    config_id: i64,
}

/// Authenticated caller who may access the `:configId` in the path.
///
/// Spec §6.6 / `Y+access`, same ACL as
/// [`crate::routes::control::access::check_read`]:
/// - `admin` bypasses `server_user_grant`.
/// - every other role needs a grant row for `configId`.
/// - missing `server_connection` → `404` (do not leak existence via 403).
#[derive(Debug, Clone)]
pub struct RequireServerAccess {
    pub user: AuthUser,
    pub connection: ServerConnection,
}

/// Resolve a `server_connection` the caller is allowed to read.
///
/// Shared by [`RequireServerAccess`] and
/// [`crate::routes::control::access::check_read`] so the grant ACL lives in
/// one place.
pub async fn resolve_server_read_access(
    db: &Database,
    user: &AuthUser,
    config_id: i64,
) -> Result<ServerConnection, AuthError> {
    let connection = server_connections::find_by_id(db, config_id)
        .await
        .map_err(|e| {
            tracing::error!(
                err = %e,
                config_id,
                "server access: server_connection lookup failed"
            );
            AuthError::Internal
        })?
        .ok_or(AuthError::NotFound)?;
    if user.is_admin() {
        return Ok(connection);
    }
    let granted = server_user_grants::exists(db, user.id, config_id)
        .await
        .map_err(|e| {
            tracing::error!(
                err = %e,
                user_id = user.id,
                config_id,
                "server access: grant lookup failed"
            );
            AuthError::Internal
        })?;
    if !granted {
        // Spec §6.4.2 / existing control surface — missing-grant ⇒ 403,
        // not 404. The connection lookup already ran, so we do not leak
        // existence.
        return Err(AuthError::Forbidden);
    }
    Ok(connection)
}

impl<S> FromRequestParts<S> for RequireServerAccess
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let RequireAuth(user) = RequireAuth::from_request_parts(parts, state).await?;
        let Path(ConfigIdParam { config_id }) =
            Path::<ConfigIdParam>::from_request_parts(parts, state)
                .await
                .map_err(|_| AuthError::InvalidServerId)?;
        let app: AppState = AppState::from_ref(state);
        let connection = resolve_server_read_access(&app.db, &user, config_id).await?;
        Ok(RequireServerAccess { user, connection })
    }
}

/// Rejection responses for the extractors above. Bodies match spec §6.4
/// verbatim via `auth_error_strings::*`.
#[derive(Debug, Clone, Copy)]
pub enum AuthError {
    NoToken,
    Invalid,
    Disabled,
    Forbidden,
    InvalidServerId,
    NotFound,
    Internal,
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            AuthError::NoToken => (StatusCode::UNAUTHORIZED, msg::NO_TOKEN),
            AuthError::Invalid => (StatusCode::UNAUTHORIZED, msg::INVALID_TOKEN),
            AuthError::Disabled => (StatusCode::UNAUTHORIZED, msg::USER_DISABLED),
            AuthError::Forbidden => (StatusCode::FORBIDDEN, msg::INSUFFICIENT_PERMS),
            AuthError::InvalidServerId => (StatusCode::BAD_REQUEST, msg::INVALID_SERVER_ID),
            AuthError::NotFound => (StatusCode::NOT_FOUND, "Not found"),
            AuthError::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "internal server error"),
        };
        (status, Json(ErrorResponse::new(msg))).into_response()
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
    use axum::Router;
    use axum::body::Body;
    use axum::http::{HeaderValue, Method, Request, StatusCode};
    use axum::routing::get;
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use std::time::Duration;
    use tower::ServiceExt;
    use ts6_manager_shared::auth::ErrorResponse;

    async fn fresh_state() -> AppState {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        crypto::init("test-seed-require-server-access");
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
        }
    }

    async fn seed_user_with_token(state: &AppState, name: &str, role: &str) -> (AuthUser, String) {
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
        (
            AuthUser {
                id: row.id,
                username: row.username,
                display_name: row.displayName,
                role: row.role,
                enabled: row.enabled,
            },
            token,
        )
    }

    async fn seed_server(state: &AppState, name: &str) -> i64 {
        crate::repos::server_connections::insert(
            &state.db,
            NewServerConnection {
                name: name.into(),
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

    async fn probe(
        RequireServerAccess { user, connection }: RequireServerAccess,
    ) -> (StatusCode, String) {
        (
            StatusCode::NO_CONTENT,
            format!("{}:{}", user.username, connection.id),
        )
    }

    fn app(state: AppState) -> Router {
        Router::new()
            .route("/api/servers/{configId}/probe", get(probe))
            .with_state(state)
    }

    fn auth_header(token: &str) -> HeaderValue {
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap()
    }

    async fn call(
        state: AppState,
        token: Option<&str>,
        config_id: i64,
    ) -> axum::http::Response<Body> {
        let mut req = Request::builder()
            .method(Method::GET)
            .uri(format!("/api/servers/{config_id}/probe"));
        if let Some(token) = token {
            req = req.header("authorization", auth_header(token));
        }
        app(state)
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn read_error(resp: axum::http::Response<Body>) -> ErrorResponse {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "expected JSON error, got {:?}: {e}",
                String::from_utf8_lossy(&bytes)
            )
        })
    }

    #[tokio::test]
    async fn admin_is_allowed_without_a_grant() {
        let state = fresh_state().await;
        let (_admin, token) = seed_user_with_token(&state, "alice", "admin").await;
        let sid = seed_server(&state, "S").await;
        let resp = call(state, Some(&token), sid).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn viewer_with_grant_is_allowed() {
        let state = fresh_state().await;
        let (viewer, token) = seed_user_with_token(&state, "viewer", "viewer").await;
        let sid = seed_server(&state, "S").await;
        server_user_grants::insert(&state.db, viewer.id, sid)
            .await
            .unwrap();
        let resp = call(state, Some(&token), sid).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn moderator_with_grant_is_allowed() {
        let state = fresh_state().await;
        let (modr, token) = seed_user_with_token(&state, "mod", "moderator").await;
        let sid = seed_server(&state, "S").await;
        server_user_grants::insert(&state.db, modr.id, sid)
            .await
            .unwrap();
        let resp = call(state, Some(&token), sid).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn viewer_without_grant_is_forbidden() {
        let state = fresh_state().await;
        let (_viewer, token) = seed_user_with_token(&state, "viewer", "viewer").await;
        let sid = seed_server(&state, "S").await;
        let resp = call(state, Some(&token), sid).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = read_error(resp).await;
        assert_eq!(body.error, msg::INSUFFICIENT_PERMS);
    }

    #[tokio::test]
    async fn moderator_without_grant_is_forbidden() {
        let state = fresh_state().await;
        let (_modr, token) = seed_user_with_token(&state, "mod", "moderator").await;
        let sid = seed_server(&state, "S").await;
        let resp = call(state, Some(&token), sid).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = read_error(resp).await;
        assert_eq!(body.error, msg::INSUFFICIENT_PERMS);
    }

    #[tokio::test]
    async fn missing_server_is_404_for_admin() {
        let state = fresh_state().await;
        let (_admin, token) = seed_user_with_token(&state, "alice", "admin").await;
        let resp = call(state, Some(&token), 9999).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = read_error(resp).await;
        assert_eq!(body.error, "Not found");
    }

    #[tokio::test]
    async fn missing_server_is_404_for_viewer() {
        let state = fresh_state().await;
        let (_viewer, token) = seed_user_with_token(&state, "viewer", "viewer").await;
        let resp = call(state, Some(&token), 9999).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = read_error(resp).await;
        assert_eq!(body.error, "Not found");
    }

    #[tokio::test]
    async fn unauthenticated_is_401() {
        let state = fresh_state().await;
        let sid = seed_server(&state, "S").await;
        let resp = call(state, None, sid).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = read_error(resp).await;
        assert_eq!(body.error, msg::NO_TOKEN);
    }
}
