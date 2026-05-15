//! Spec §6.4.1 / §6.4.2 — Axum extractors for authentication and role gating.
//!
//! [`RequireAuth`] is the canonical extractor: it parses the `Authorization:
//! Bearer <jwt>` header, verifies the JWT, looks up the user row in SurrealDB,
//! and returns an [`AuthUser`]. **The role used downstream comes from the DB
//! lookup, not the JWT claim** (spec §6.4.1) — revoking a user's role takes
//! effect immediately.
//!
//! [`RequireRole`] composes on top of `RequireAuth` to gate routes by role
//! membership. [`crate::auth::extractors`] does NOT yet ship the per-server
//! `RequireServerAccess` extractor — that lands when the first per-server
//! REST route does (none exist yet in the Phase 1 surface).

use std::convert::Infallible;
use std::net::SocketAddr;

use axum::Json;
use axum::extract::{ConnectInfo, FromRef, FromRequestParts};
use axum::http::StatusCode;
use axum::http::header::{AUTHORIZATION, USER_AGENT};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use ts6_manager_shared::auth::{ErrorResponse, auth_error_strings as msg};

use crate::app_state::AppState;
use crate::auth::jwt;
use crate::repos::users;
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

/// Rejection responses for the extractors above. Bodies match spec §6.4
/// verbatim via `auth_error_strings::*`.
#[derive(Debug, Clone, Copy)]
pub enum AuthError {
    NoToken,
    Invalid,
    Disabled,
    Forbidden,
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            AuthError::NoToken => (StatusCode::UNAUTHORIZED, msg::NO_TOKEN),
            AuthError::Invalid => (StatusCode::UNAUTHORIZED, msg::INVALID_TOKEN),
            AuthError::Disabled => (StatusCode::UNAUTHORIZED, msg::USER_DISABLED),
            AuthError::Forbidden => (StatusCode::FORBIDDEN, msg::INSUFFICIENT_PERMS),
        };
        (status, Json(ErrorResponse::new(msg))).into_response()
    }
}
