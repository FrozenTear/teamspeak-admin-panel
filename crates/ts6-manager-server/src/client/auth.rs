//! Typed REST client for `/api/auth/*` (spec §6.5 + §7).
//!
//! Each function wraps one endpoint and (de)serialises through the
//! `ts6_manager_shared::auth` types — JSON keys are the source of truth, so
//! this module never builds JSON by hand.
//!
//! Transport is `gloo-net` on WASM. On native (server-side compile) the
//! functions exist for typecheck but unconditionally return
//! [`AuthError::UnsupportedTarget`] — server SSR doesn't perform an auth
//! request, and tests exercise the higher-level state machines through
//! mockable traits in [`crate::client::session`] instead of hitting fetch.

use serde::Serialize;
use serde::de::DeserializeOwned;
use ts6_manager_shared::auth::{
    ChangePasswordRequest, ErrorResponse, LoginRequest, LogoutRequest, RefreshRequest,
    TokenPairResponse, UserInfo, auth_error_strings as msg,
};

/// Error returned by every client function.
///
/// `Unauthorized` carries the spec error string verbatim so the refresh
/// interceptor can detect `Invalid or expired token` and trigger rotation
/// without re-parsing the body. `Other` covers transport/parse errors and
/// any 4xx/5xx that isn't a 401-with-`Invalid or expired token`.
#[derive(Debug, Clone)]
pub enum AuthError {
    /// HTTP 401 from the server. The carried string is the spec error body
    /// (e.g. [`msg::INVALID_TOKEN`] or [`msg::USER_DISABLED`]).
    Unauthorized(String),
    /// HTTP 4xx other than 401, with the server's error string if parseable.
    Client { status: u16, message: String },
    /// HTTP 5xx.
    Server { status: u16, message: String },
    /// Transport error (network, CORS, abort, etc.).
    Transport(String),
    /// Response was not valid JSON or didn't match the wire shape.
    Deserialise(String),
    /// Auth client called from a non-WASM target. Server SSR never hits the
    /// network for `/api/auth/*` directly; the binary mints/validates tokens
    /// in-process. Only present so the API typechecks on every target.
    UnsupportedTarget,
}

impl AuthError {
    /// Spec §6.4.1: the refresh interceptor only retries when the body is
    /// exactly [`msg::INVALID_TOKEN`]. Other 401s (e.g.
    /// [`msg::USER_DISABLED`]) terminate the session.
    pub fn is_invalid_or_expired_token(&self) -> bool {
        matches!(self, AuthError::Unauthorized(s) if s == msg::INVALID_TOKEN)
    }

    /// `true` for any 401, regardless of body.
    pub fn is_unauthorized(&self) -> bool {
        matches!(self, AuthError::Unauthorized(_))
    }
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::Unauthorized(m) => write!(f, "401 Unauthorized: {m}"),
            AuthError::Client { status, message } => write!(f, "{status}: {message}"),
            AuthError::Server { status, message } => write!(f, "{status}: {message}"),
            AuthError::Transport(m) => write!(f, "transport error: {m}"),
            AuthError::Deserialise(m) => write!(f, "deserialise error: {m}"),
            AuthError::UnsupportedTarget => write!(f, "auth client unsupported on this target"),
        }
    }
}

impl std::error::Error for AuthError {}

/// `POST /api/auth/login` — exchange username/password for a token pair.
pub async fn login(base: &str, req: &LoginRequest) -> Result<TokenPairResponse, AuthError> {
    request_json(base, "POST", "/api/auth/login", None, Some(req)).await
}

/// `POST /api/auth/refresh` — rotate the refresh token, mint a new access.
pub async fn refresh(base: &str, req: &RefreshRequest) -> Result<TokenPairResponse, AuthError> {
    request_json(base, "POST", "/api/auth/refresh", None, Some(req)).await
}

/// `POST /api/auth/logout` — server returns 204; we collapse to `()`.
///
/// Spec §6.5.5: idempotent — server returns 204 whether or not the token
/// existed. The client treats 401 the same way the server does (token gone)
/// so a stale-session logout is never user-visible.
pub async fn logout(base: &str, req: &LogoutRequest) -> Result<(), AuthError> {
    match request_no_content(base, "POST", "/api/auth/logout", None, Some(req)).await {
        Ok(()) => Ok(()),
        // Tolerate 401: server-side token already gone. Spec §6.5.5
        // documents idempotency; treating 401 as "already logged out" makes
        // the client's logout button equally idempotent.
        Err(AuthError::Unauthorized(_)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// `GET /api/auth/me` — the current user's profile, identified by the bearer.
pub async fn me(base: &str, access_token: &str) -> Result<UserInfo, AuthError> {
    request_json::<(), _>(base, "GET", "/api/auth/me", Some(access_token), None).await
}

/// `PUT /api/auth/password` — server returns 204 on success.
///
/// Server-side this also revokes every refresh token for the user (spec
/// §6.2.3); after a successful call the caller should treat the current
/// refresh token as gone and force re-login.
pub async fn change_password(
    base: &str,
    access_token: &str,
    req: &ChangePasswordRequest,
) -> Result<(), AuthError> {
    request_no_content(
        base,
        "PUT",
        "/api/auth/password",
        Some(access_token),
        Some(req),
    )
    .await
}

// ---------------------------------------------------------------------------
// Transport — gloo-net on WASM, panic-on-call on native.

#[cfg(target_arch = "wasm32")]
async fn request_json<Req, Resp>(
    base: &str,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Option<&Req>,
) -> Result<Resp, AuthError>
where
    Req: Serialize,
    Resp: DeserializeOwned,
{
    let resp = send(base, method, path, bearer, body).await?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| AuthError::Transport(e.to_string()))?;
    if status >= 200 && status < 300 {
        serde_json::from_str(&text).map_err(|e| AuthError::Deserialise(e.to_string()))
    } else {
        Err(map_error_status(status, &text))
    }
}

#[cfg(not(target_arch = "wasm32"))]
async fn request_json<Req, Resp>(
    _base: &str,
    _method: &str,
    _path: &str,
    _bearer: Option<&str>,
    _body: Option<&Req>,
) -> Result<Resp, AuthError>
where
    Req: Serialize,
    Resp: DeserializeOwned,
{
    Err(AuthError::UnsupportedTarget)
}

#[cfg(target_arch = "wasm32")]
async fn request_no_content<Req: Serialize>(
    base: &str,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Option<&Req>,
) -> Result<(), AuthError> {
    let resp = send(base, method, path, bearer, body).await?;
    let status = resp.status();
    if status == 204 || (200..300).contains(&status) {
        Ok(())
    } else {
        let text = resp.text().await.unwrap_or_default();
        Err(map_error_status(status, &text))
    }
}

#[cfg(not(target_arch = "wasm32"))]
async fn request_no_content<Req: Serialize>(
    _base: &str,
    _method: &str,
    _path: &str,
    _bearer: Option<&str>,
    _body: Option<&Req>,
) -> Result<(), AuthError> {
    Err(AuthError::UnsupportedTarget)
}

#[cfg(target_arch = "wasm32")]
async fn send<Req: Serialize>(
    base: &str,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Option<&Req>,
) -> Result<gloo_net::http::Response, AuthError> {
    use gloo_net::http::Request;
    let url = format!("{}{}", base.trim_end_matches('/'), path);
    let mut builder = match method {
        "GET" => Request::get(&url),
        "POST" => Request::post(&url),
        "PUT" => Request::put(&url),
        "DELETE" => Request::delete(&url),
        other => panic!("unsupported HTTP method `{other}`"),
    };
    if let Some(token) = bearer {
        builder = builder.header("authorization", &format!("Bearer {token}"));
    }
    let request = if let Some(b) = body {
        builder
            .header("content-type", "application/json")
            .json(b)
            .map_err(|e| AuthError::Transport(e.to_string()))?
    } else {
        builder
            .build()
            .map_err(|e| AuthError::Transport(e.to_string()))?
    };
    request
        .send()
        .await
        .map_err(|e| AuthError::Transport(e.to_string()))
}

/// Map an HTTP status + body into the right [`AuthError`] variant. The
/// spec error envelope is `{ "error": "..." }` — anything else falls through
/// to the raw body so debug logs are still meaningful.
fn map_error_status(status: u16, body: &str) -> AuthError {
    let message = serde_json::from_str::<ErrorResponse>(body)
        .map(|e| e.error)
        .unwrap_or_else(|_| body.to_string());
    match status {
        401 => AuthError::Unauthorized(message),
        s if (400..500).contains(&s) => AuthError::Client { status: s, message },
        s if (500..600).contains(&s) => AuthError::Server { status: s, message },
        s => AuthError::Server { status: s, message },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_error_status_extracts_spec_error_envelope() {
        let body = r#"{"error":"Invalid or expired token"}"#;
        let e = map_error_status(401, body);
        assert!(e.is_invalid_or_expired_token(), "got: {e}");
    }

    #[test]
    fn map_error_status_distinguishes_disabled_user_from_invalid_token() {
        let body = r#"{"error":"User account disabled or deleted"}"#;
        let e = map_error_status(401, body);
        // 401 — but NOT the "Invalid or expired token" path the refresh
        // interceptor watches for.
        assert!(e.is_unauthorized());
        assert!(!e.is_invalid_or_expired_token(), "got: {e}");
    }

    #[test]
    fn map_error_status_falls_back_to_raw_body_when_envelope_absent() {
        let body = "<html>500</html>";
        match map_error_status(500, body) {
            AuthError::Server { status, message } => {
                assert_eq!(status, 500);
                assert_eq!(message, "<html>500</html>");
            }
            other => panic!("expected Server, got {other:?}"),
        }
    }

    #[test]
    fn map_error_status_429_lands_in_client_bucket() {
        let body = r#"{"error":"Too many attempts, please try again later"}"#;
        match map_error_status(429, body) {
            AuthError::Client { status, message } => {
                assert_eq!(status, 429);
                assert_eq!(message, msg::RATE_LIMIT_AUTH);
            }
            other => panic!("expected Client, got {other:?}"),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn native_callers_get_unsupported_target() {
        let err = login(
            "http://example",
            &LoginRequest {
                username: "x".into(),
                password: "y".into(),
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AuthError::UnsupportedTarget));
    }
}
