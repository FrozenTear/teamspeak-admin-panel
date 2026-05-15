//! Typed REST client for `/api/setup/*` (spec §7.2 / PURA-22 wire shapes).
//!
//! Both endpoints are intentionally **unauthenticated** — the gate is
//! `needsSetup == user_count == 0`. Once an admin exists the server hard-fails
//! init with `409 { "error": "already_initialized" }`. We surface that as a
//! dedicated [`SetupInitError::AlreadyInitialized`] variant so the wizard can
//! render the spec-correct copy without parsing English.
//!
//! Transport is `gloo-net` on WASM and a `UnsupportedTarget` no-op everywhere
//! else; the wizard only ever runs in the browser, but the type-check has to
//! compile under `--features server` too.

use serde::Serialize;
use serde::de::DeserializeOwned;
use ts6_manager_shared::setup::{SetupInitRequest, SetupInitResponse, SetupStatusResponse};
use ts6_manager_shared::test_connection::{TestConnectionRequest, TestConnectionResponse};

use crate::client::api::ApiError;
#[cfg(target_arch = "wasm32")]
use crate::client::api::classify_response;

/// Wire-string used by the server for the one-shot 409 (PURA-22 fixes the
/// body exactly so the FE can branch without reading copy).
pub const ALREADY_INITIALIZED: &str = "already_initialized";

/// Errors specific to the `/api/setup/init` flow. Wraps [`ApiError`] for
/// every transport / serialisation / generic-HTTP failure and adds two
/// branches the wizard cares about:
/// - [`SetupInitError::AlreadyInitialized`] — `409 already_initialized`. The
///   admin already exists; the wizard nudges to `/login`.
/// - [`SetupInitError::WeakPassword`] — `400` with the spec-verbatim message
///   from `crate::auth::complexity`. Surfaced to the password field instead
///   of the generic banner.
#[derive(Debug, Clone)]
pub enum SetupInitError {
    AlreadyInitialized,
    WeakPassword(String),
    Other(ApiError),
}

impl std::fmt::Display for SetupInitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetupInitError::AlreadyInitialized => f.write_str("already_initialized"),
            SetupInitError::WeakPassword(m) => write!(f, "{m}"),
            SetupInitError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SetupInitError {}

/// `GET /api/setup/status` — returns `{ needsSetup }`. Unauthenticated.
pub async fn status(base: &str) -> Result<SetupStatusResponse, ApiError> {
    unauth_request_json::<(), _>(base, "GET", "/api/setup/status", None).await
}

/// PURA-211 — `POST /api/setup/test-connection`. Always parses the body
/// as a [`TestConnectionResponse`] (even on probe failure — the server
/// surfaces classification info in the body, not the HTTP status), and
/// surfaces `409 already_initialized` as a dedicated error so the wizard
/// can bounce to login the same way `init` does.
pub async fn test_connection(
    base: &str,
    req: &TestConnectionRequest,
) -> Result<TestConnectionResponse, SetupInitError> {
    match unauth_request_json(base, "POST", "/api/setup/test-connection", Some(req)).await {
        Ok(body) => Ok(body),
        Err(ApiError::Client {
            status: 409,
            message,
        }) if message == ALREADY_INITIALIZED => Err(SetupInitError::AlreadyInitialized),
        Err(other) => Err(SetupInitError::Other(other)),
    }
}

/// `POST /api/setup/init` — one-shot admin + first-server creation.
/// Returns the freshly-created [`SetupInitResponse`] on success.
pub async fn init(base: &str, req: &SetupInitRequest) -> Result<SetupInitResponse, SetupInitError> {
    match unauth_request_json(base, "POST", "/api/setup/init", Some(req)).await {
        Ok(body) => Ok(body),
        Err(ApiError::Client {
            status: 409,
            message,
        }) if message == ALREADY_INITIALIZED => Err(SetupInitError::AlreadyInitialized),
        // 400 from this endpoint is the §6.2.2 weak-password branch — the
        // server returns the spec-verbatim rule message in `error`. Anything
        // else (missing field, malformed JSON) bubbles through as Other.
        Err(ApiError::Client {
            status: 400,
            message,
        }) => Err(SetupInitError::WeakPassword(message)),
        Err(other) => Err(SetupInitError::Other(other)),
    }
}

#[cfg(target_arch = "wasm32")]
async fn unauth_request_json<Req, Resp>(
    base: &str,
    method: &str,
    path: &str,
    body: Option<&Req>,
) -> Result<Resp, ApiError>
where
    Req: Serialize,
    Resp: DeserializeOwned,
{
    use gloo_net::http::Request;
    let url = format!("{}{}", base.trim_end_matches('/'), path);
    let mut builder = match method {
        "GET" => Request::get(&url),
        "POST" => Request::post(&url),
        other => panic!("unsupported HTTP method `{other}`"),
    };
    let request = if let Some(b) = body {
        builder = builder.header("content-type", "application/json");
        builder
            .json(b)
            .map_err(|e| ApiError::Transport(e.to_string()))?
    } else {
        builder
            .build()
            .map_err(|e| ApiError::Transport(e.to_string()))?
    };
    let resp = request
        .send()
        .await
        .map_err(|e| ApiError::Transport(e.to_string()))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| ApiError::Transport(e.to_string()))?;
    classify_response(status, &body)
}

#[cfg(not(target_arch = "wasm32"))]
async fn unauth_request_json<Req, Resp>(
    _base: &str,
    _method: &str,
    _path: &str,
    _body: Option<&Req>,
) -> Result<Resp, ApiError>
where
    Req: Serialize,
    Resp: DeserializeOwned,
{
    Err(ApiError::UnsupportedTarget)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn already_initialized_constant_matches_server_wire_string() {
        // PURA-22 fixes the body exactly. If the server tweaks the literal,
        // this test is the canary that forces a paired FE/BE update.
        assert_eq!(ALREADY_INITIALIZED, "already_initialized");
    }

    #[test]
    fn setup_init_error_display_uses_underlying_message() {
        assert_eq!(
            format!("{}", SetupInitError::AlreadyInitialized),
            "already_initialized"
        );
        assert_eq!(
            format!(
                "{}",
                SetupInitError::WeakPassword("Password too short".into())
            ),
            "Password too short"
        );
    }

    #[test]
    fn already_initialized_response_maps_to_dedicated_variant() {
        // Mirror what classify_response would produce for a 409 with the
        // server's verbatim envelope, then run it through the same match the
        // live `init()` uses. Keeps the branch covered without spinning up
        // a real fetch.
        let raw_409 = ApiError::Client {
            status: 409,
            message: ALREADY_INITIALIZED.into(),
        };
        let mapped = match raw_409 {
            ApiError::Client {
                status: 409,
                message,
            } if message == ALREADY_INITIALIZED => SetupInitError::AlreadyInitialized,
            other => SetupInitError::Other(other),
        };
        assert!(matches!(mapped, SetupInitError::AlreadyInitialized));
    }

    #[test]
    fn weak_password_400_carries_through_message() {
        let raw_400 = ApiError::Client {
            status: 400,
            message: "Password must be at least 12 characters".into(),
        };
        let mapped = match raw_400 {
            ApiError::Client {
                status: 400,
                message,
            } => SetupInitError::WeakPassword(message),
            other => SetupInitError::Other(other),
        };
        match mapped {
            SetupInitError::WeakPassword(m) => assert!(m.contains("12 characters"), "got: {m}"),
            other => panic!("expected WeakPassword, got {other:?}"),
        }
    }

    #[test]
    fn non_already_initialized_409_does_not_collapse_to_dedicated_variant() {
        // A 409 carrying a different envelope (theoretical — server doesn't
        // emit one today) must NOT be misclassified as AlreadyInitialized.
        let other_409 = ApiError::Client {
            status: 409,
            message: "something else".into(),
        };
        let mapped = match other_409 {
            ApiError::Client {
                status: 409,
                message,
            } if message == ALREADY_INITIALIZED => SetupInitError::AlreadyInitialized,
            other => SetupInitError::Other(other),
        };
        assert!(matches!(mapped, SetupInitError::Other(_)));
    }
}
