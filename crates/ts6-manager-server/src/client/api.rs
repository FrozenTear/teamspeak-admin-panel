//! Authorized JSON fetch helper for the operator SPA.
//!
//! Wraps `gloo-net` with the [`RefreshGate`] so any caller gets transparent
//! access-token refresh on `401 Invalid or expired token`. Non-401 errors —
//! including the spec §7.0.2 `502` envelope (`{ error, code, details }`) for
//! TeamSpeak upstream failures — are surfaced verbatim through [`ApiError`]
//! so the UI can render the right banner/state.
//!
//! Auth-flow endpoints (`/api/auth/*`) keep their own typed client in
//! [`crate::client::auth`]: those calls don't hold a session yet, and the
//! refresh interceptor is the very thing the auth surface bootstraps.
//! Everything else — dashboard counts, server lists, future per-route
//! data fetches — should funnel through [`authorized_get_json`] here so the
//! single-flight refresh contract holds across the SPA.

use serde::Deserialize;
use serde::de::DeserializeOwned;
use ts6_manager_shared::auth::ErrorResponse;

use crate::client::auth::AuthError;
use crate::client::session::{RefreshGate, SessionSnapshot};

/// Errors surfaced to UI callers.
///
/// `Unauthorized` means the gate exhausted its single refresh attempt and the
/// session is now anonymous — the caller should rely on `AppShell`'s
/// auth-gate effect to bounce the user to `/login`. `BadGateway` carries the
/// spec §7.0.2 envelope so the dashboard (and any other surface that fans
/// requests through to TeamSpeak) can render the upstream's diagnostic
/// message instead of a generic "something went wrong".
#[derive(Debug, Clone)]
pub enum ApiError {
    /// 401 from a non-auth endpoint after a single failed refresh attempt,
    /// or 401 with a non-`Invalid or expired token` body (e.g. user
    /// disabled). Either way: re-auth required.
    Unauthorized(String),

    /// Spec §7.0.2 502 envelope — TeamSpeak upstream failed.
    /// `code` follows the WebQuery numeric scheme; `-1` is the panel-internal
    /// "transport / TLS / decrypt failure" sentinel (§10.5).
    BadGateway {
        error: String,
        code: Option<i64>,
        details: Option<String>,
    },

    /// 4xx other than 401, with the server's `{"error": ...}` envelope when
    /// parseable.
    Client { status: u16, message: String },

    /// 5xx other than 502 — the server itself failed before reaching the TS
    /// upstream.
    Server { status: u16, message: String },

    /// Network / CORS / abort.
    Transport(String),

    /// Body wasn't the expected shape.
    Deserialise(String),

    /// Called from a non-WASM build target. SSR + native tests never run a
    /// real fetch through this helper; this variant exists so the API
    /// type-checks on every target the workspace builds.
    UnsupportedTarget,
}

impl ApiError {
    pub fn is_unauthorized(&self) -> bool {
        matches!(self, ApiError::Unauthorized(_))
    }

    pub fn is_bad_gateway(&self) -> bool {
        matches!(self, ApiError::BadGateway { .. })
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Unauthorized(m) => write!(f, "401 Unauthorized: {m}"),
            ApiError::BadGateway {
                error,
                code,
                details,
            } => {
                write!(f, "502 {error}")?;
                if let Some(c) = code {
                    write!(f, " (code {c})")?;
                }
                if let Some(d) = details {
                    write!(f, ": {d}")?;
                }
                Ok(())
            }
            ApiError::Client { status, message } | ApiError::Server { status, message } => {
                write!(f, "{status}: {message}")
            }
            ApiError::Transport(m) => write!(f, "transport error: {m}"),
            ApiError::Deserialise(m) => write!(f, "deserialise error: {m}"),
            ApiError::UnsupportedTarget => write!(f, "api client unsupported on this target"),
        }
    }
}

impl std::error::Error for ApiError {}

impl From<AuthError> for ApiError {
    fn from(err: AuthError) -> Self {
        match err {
            AuthError::Unauthorized(m) => ApiError::Unauthorized(m),
            AuthError::Client { status, message } => ApiError::Client { status, message },
            AuthError::Server { status, message } => ApiError::Server { status, message },
            AuthError::Transport(m) => ApiError::Transport(m),
            AuthError::Deserialise(m) => ApiError::Deserialise(m),
            AuthError::UnsupportedTarget => ApiError::UnsupportedTarget,
        }
    }
}

/// Origin of the API server. On WASM this is the same origin the SPA was
/// served from; on native (SSR / unit tests) we return an empty string —
/// no production code path actually issues a request through this helper
/// off-WASM, but the function exists so callers don't need their own cfg
/// gymnastics.
pub fn api_base() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            if let Ok(origin) = window.location().origin() {
                return origin;
            }
        }
        String::new()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        String::new()
    }
}

/// `GET {base}{path}` with the active access token attached. The
/// [`RefreshGate`] handles single-flight refresh-on-401 transparently.
pub async fn authorized_get_json<T>(
    gate: &RefreshGate,
    base: &str,
    path: &str,
) -> Result<T, ApiError>
where
    T: DeserializeOwned,
{
    let (status, body) = gate
        .run(|snap| {
            let base = base.to_owned();
            let path = path.to_owned();
            async move { authorized_get_raw(&base, &path, &snap).await }
        })
        .await
        .map_err(ApiError::from)?;

    classify_response(status, &body)
}

/// `POST {base}{path}` with an optional JSON body and refresh-gating.
///
/// Pass `None` for a body-less POST (e.g. `unmute`). The control surface
/// handlers in [`crate::routes::control`] return `204 No Content` on
/// success — pass `()` for `T` to discard the empty body, or a typed
/// payload for handlers that respond with JSON (`POST .../bans` → 201
/// `{ banid }`).
pub async fn authorized_post_json<B, T>(
    gate: &RefreshGate,
    base: &str,
    path: &str,
    body: Option<&B>,
) -> Result<T, ApiError>
where
    B: serde::Serialize + ?Sized,
    T: DeserializeOwned,
{
    let body_string = body
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| ApiError::Deserialise(e.to_string()))?;
    let (status, body) = gate
        .run(|snap| {
            let base = base.to_owned();
            let path = path.to_owned();
            let body_string = body_string.clone();
            async move {
                authorized_send_raw(
                    HttpMethod::Post,
                    &base,
                    &path,
                    body_string.as_deref(),
                    &snap,
                )
                .await
            }
        })
        .await
        .map_err(ApiError::from)?;

    classify_maybe_empty(status, &body)
}

/// `DELETE {base}{path}` with refresh-gating. 204 → `Ok(())`.
pub async fn authorized_delete(
    gate: &RefreshGate,
    base: &str,
    path: &str,
) -> Result<(), ApiError> {
    let (status, body) = gate
        .run(|snap| {
            let base = base.to_owned();
            let path = path.to_owned();
            async move { authorized_send_raw(HttpMethod::Delete, &base, &path, None, &snap).await }
        })
        .await
        .map_err(ApiError::from)?;

    classify_maybe_empty::<()>(status, &body)
}

/// `POST` / `DELETE` body-less variant: `204 No Content` is treated as
/// success when `T = ()`, and any 2xx body is parsed as JSON otherwise.
/// Non-2xx responses go through [`classify_response`] for the spec §7.0.2
/// error-envelope handling.
pub(crate) fn classify_maybe_empty<T: DeserializeOwned>(status: u16, body: &str) -> Result<T, ApiError> {
    if (200..300).contains(&status) {
        if status == 204 || body.trim().is_empty() {
            // For T = () this resolves to Ok(()). For typed payloads this
            // is a programmer error — the route should have returned a
            // body — so a deserialise failure here is the right surface.
            return serde_json::from_str("null")
                .map_err(|e| ApiError::Deserialise(e.to_string()));
        }
        return serde_json::from_str(body).map_err(|e| ApiError::Deserialise(e.to_string()));
    }
    classify_response::<T>(status, body)
}

/// Parse a (status, body) pair into a typed result, applying the spec §7.0.2
/// envelope rules. Pulled out as a free function so it can be unit-tested
/// without touching `gloo-net`, and reused by the unauth setup module which
/// inherits the same `{error}`-envelope contract for non-2xx responses.
pub(crate) fn classify_response<T: DeserializeOwned>(status: u16, body: &str) -> Result<T, ApiError> {
    if (200..300).contains(&status) {
        return serde_json::from_str(body).map_err(|e| ApiError::Deserialise(e.to_string()));
    }
    if status == 502 {
        let env: BadGatewayBody = serde_json::from_str(body).unwrap_or_default();
        return Err(ApiError::BadGateway {
            error: env.error.unwrap_or_else(|| "TeamSpeak API Error".into()),
            code: env.code,
            details: env.details,
        });
    }
    let message = serde_json::from_str::<ErrorResponse>(body)
        .map(|e| e.error)
        .unwrap_or_else(|_| body.to_string());
    if (400..500).contains(&status) {
        Err(ApiError::Client { status, message })
    } else {
        Err(ApiError::Server { status, message })
    }
}

/// `{ error, code?, details? }` — spec §7.0.2 wire shape used by the
/// dashboard route's WebQuery upstream errors.
#[derive(Debug, Default, Deserialize)]
struct BadGatewayBody {
    error: Option<String>,
    code: Option<i64>,
    details: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum HttpMethod {
    Post,
    Delete,
}

#[cfg(target_arch = "wasm32")]
async fn authorized_get_raw(
    base: &str,
    path: &str,
    snap: &SessionSnapshot,
) -> Result<(u16, String), AuthError> {
    use gloo_net::http::Request;
    let url = format!("{}{}", base.trim_end_matches('/'), path);
    let resp = Request::get(&url)
        .header("authorization", &format!("Bearer {}", snap.access))
        .send()
        .await
        .map_err(|e| AuthError::Transport(e.to_string()))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| AuthError::Transport(e.to_string()))?;
    if status == 401 {
        let msg = serde_json::from_str::<ErrorResponse>(&body)
            .map(|e| e.error)
            .unwrap_or_else(|_| body.clone());
        return Err(AuthError::Unauthorized(msg));
    }
    Ok((status, body))
}

#[cfg(not(target_arch = "wasm32"))]
async fn authorized_get_raw(
    _base: &str,
    _path: &str,
    _snap: &SessionSnapshot,
) -> Result<(u16, String), AuthError> {
    Err(AuthError::UnsupportedTarget)
}

#[cfg(target_arch = "wasm32")]
async fn authorized_send_raw(
    method: HttpMethod,
    base: &str,
    path: &str,
    body: Option<&str>,
    snap: &SessionSnapshot,
) -> Result<(u16, String), AuthError> {
    use gloo_net::http::Request;
    let url = format!("{}{}", base.trim_end_matches('/'), path);
    let mut builder = match method {
        HttpMethod::Post => Request::post(&url),
        HttpMethod::Delete => Request::delete(&url),
    };
    builder = builder.header("authorization", &format!("Bearer {}", snap.access));
    let resp = if let Some(b) = body {
        builder = builder.header("content-type", "application/json");
        builder
            .body(b.to_string())
            .map_err(|e| AuthError::Transport(e.to_string()))?
            .send()
            .await
    } else {
        builder.send().await
    }
    .map_err(|e| AuthError::Transport(e.to_string()))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| AuthError::Transport(e.to_string()))?;
    if status == 401 {
        let msg = serde_json::from_str::<ErrorResponse>(&body)
            .map(|e| e.error)
            .unwrap_or_else(|_| body.clone());
        return Err(AuthError::Unauthorized(msg));
    }
    Ok((status, body))
}

#[cfg(not(target_arch = "wasm32"))]
async fn authorized_send_raw(
    _method: HttpMethod,
    _base: &str,
    _path: &str,
    _body: Option<&str>,
    _snap: &SessionSnapshot,
) -> Result<(u16, String), AuthError> {
    Err(AuthError::UnsupportedTarget)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Demo {
        name: String,
    }

    #[test]
    fn classify_2xx_decodes_into_t() {
        let demo: Demo = classify_response(200, r#"{"name":"hi"}"#).unwrap();
        assert_eq!(demo, Demo { name: "hi".into() });
    }

    #[test]
    fn classify_2xx_deserialise_error_surfaces_message() {
        let err = classify_response::<Demo>(200, "not json").unwrap_err();
        assert!(matches!(err, ApiError::Deserialise(_)), "got: {err:?}");
    }

    #[test]
    fn classify_502_extracts_spec_envelope() {
        let body = r#"{"error":"TeamSpeak API Error","code":1153,"details":"invalid serverID"}"#;
        let err = classify_response::<Demo>(502, body).unwrap_err();
        match err {
            ApiError::BadGateway {
                error,
                code,
                details,
            } => {
                assert_eq!(error, "TeamSpeak API Error");
                assert_eq!(code, Some(1153));
                assert_eq!(details.as_deref(), Some("invalid serverID"));
            }
            other => panic!("expected BadGateway, got {other:?}"),
        }
    }

    #[test]
    fn classify_502_with_internal_sentinel_code_minus_one() {
        // Spec §10.5: -1 is the panel-internal "transport/TLS/decrypt
        // failure" sentinel.
        let body = r#"{"error":"TeamSpeak API Error","code":-1,"details":"connection refused"}"#;
        let err = classify_response::<Demo>(502, body).unwrap_err();
        match err {
            ApiError::BadGateway { code, details, .. } => {
                assert_eq!(code, Some(-1));
                assert_eq!(details.as_deref(), Some("connection refused"));
            }
            other => panic!("expected BadGateway, got {other:?}"),
        }
    }

    #[test]
    fn classify_502_falls_back_to_default_error_when_envelope_missing() {
        let err = classify_response::<Demo>(502, "<html>oops</html>").unwrap_err();
        match err {
            ApiError::BadGateway { error, .. } => {
                assert_eq!(error, "TeamSpeak API Error");
            }
            other => panic!("expected BadGateway, got {other:?}"),
        }
    }

    #[test]
    fn classify_4xx_extracts_error_envelope_into_client_variant() {
        let err = classify_response::<Demo>(404, r#"{"error":"Not found"}"#).unwrap_err();
        match err {
            ApiError::Client { status, message } => {
                assert_eq!(status, 404);
                assert_eq!(message, "Not found");
            }
            other => panic!("expected Client, got {other:?}"),
        }
    }

    #[test]
    fn classify_5xx_lands_in_server_variant() {
        let err = classify_response::<Demo>(500, r#"{"error":"Internal server error"}"#)
            .unwrap_err();
        match err {
            ApiError::Server { status, message } => {
                assert_eq!(status, 500);
                assert_eq!(message, "Internal server error");
            }
            other => panic!("expected Server, got {other:?}"),
        }
    }

    #[test]
    fn auth_error_unauthorized_maps_to_api_unauthorized() {
        let api: ApiError = AuthError::Unauthorized("Invalid or expired token".into()).into();
        assert!(api.is_unauthorized(), "got: {api:?}");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn native_callers_get_unsupported_target_through_the_gate() {
        use crate::client::session::{RefreshGate, testing::InMemorySession};
        use crate::client::storage::MemoryStore;
        use crate::client::store::AuthState;
        use std::sync::Arc;
        use ts6_manager_shared::auth::UserInfo;

        struct ExplodingRefresh;
        impl crate::client::session::RefreshFn for ExplodingRefresh {
            fn refresh(
                &self,
                _: String,
            ) -> futures::future::BoxFuture<
                'static,
                Result<ts6_manager_shared::auth::TokenPairResponse, AuthError>,
            > {
                Box::pin(async { panic!("must not refresh") })
            }
        }

        let storage: Arc<dyn crate::client::storage::Storage + Send + Sync> =
            Arc::new(MemoryStore::new());
        let session: Arc<dyn crate::client::session::SessionHandle> =
            Arc::new(InMemorySession::new(
                AuthState::Authenticated {
                    access: "ax".into(),
                    refresh: "rx".into(),
                    user: UserInfo {
                        id: 1,
                        username: "u".into(),
                        display_name: "u".into(),
                        role: "admin".into(),
                    },
                },
                storage,
            ));
        let gate = RefreshGate::new(session, Arc::new(ExplodingRefresh));

        let err = authorized_get_json::<Demo>(&gate, "http://example", "/api/x")
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::UnsupportedTarget), "got: {err:?}");
    }
}
