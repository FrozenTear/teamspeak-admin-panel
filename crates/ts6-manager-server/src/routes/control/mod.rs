//! Phase 2 control surface — `/api/servers/{configId}/vs/{sid}/...` REST
//! endpoints (PURA-71).
//!
//! Each endpoint:
//! - Authenticates via the [`crate::auth::extractors::RequireAuth`] extractor
//!   chain (JWT + DB user lookup, spec §6.4.1). Inside the handler we run an
//!   additional per-server access check via [`access::check_read`] / [`access::check_write`].
//! - Resolves the `server_connection` row by `configId` and pulls a
//!   `WebQueryClient` from [`crate::app_state::AppState::webquery`].
//! - Calls the typed WebQuery surface that landed in PURA-68. SSHBridge
//!   wiring is owned by [PURA-78] (blocked on PURA-76 + PURA-77); this
//!   module is WebQuery-only per the PURA-71 issue scope.
//! - On write success, emits a `tracing::info!` audit event under
//!   `target = "control::audit"` (see [`audit`]) AND publishes a
//!   `server:{configId}:clients` / `server:{configId}:channels` event on
//!   [`crate::app_state::AppState::ws_hub`] for live propagation.
//!
//! Errors map per spec §7.0.2 — see [`translate_webquery_error`].
//!
//! Phase 2 does NOT add a per-server backend selector here. PURA-78 will
//! introduce a `ControlBackend` trait that wraps WebQuery vs. SSHBridge
//! and replace `state.webquery.get_or_build` with `backend_for(server)` —
//! the call sites are already shaped around a single typed-handle return,
//! so the swap is local to this module.

use axum::Json;
use axum::Router;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde::{Deserialize, Serialize};

use crate::app_state::AppState;
use crate::webquery::WebQueryError;

pub mod access;
pub mod audit;
pub mod bans;
pub mod channels;
pub mod clients;
pub mod info;
pub mod logs;

#[cfg(test)]
mod tests;

/// Build the control sub-router. Caller mounts via `Router::merge` so the
/// absolute paths line up with spec §7.x naming.
pub fn router() -> Router<AppState> {
    Router::new()
        // Reads.
        .route(
            "/api/servers/{configId}/vs/{sid}/clients",
            get(clients::list),
        )
        .route(
            "/api/servers/{configId}/vs/{sid}/clients/{cldbid}",
            get(clients::detail),
        )
        .route(
            "/api/servers/{configId}/vs/{sid}/channels",
            get(channels::list),
        )
        .route(
            "/api/servers/{configId}/vs/{sid}/info",
            get(info::server_info),
        )
        .route("/api/servers/{configId}/vs/{sid}/logs", get(logs::tail))
        .route("/api/servers/{configId}/vs/{sid}/bans", get(bans::list).post(bans::create))
        .route(
            "/api/servers/{configId}/vs/{sid}/bans/{banid}",
            delete(bans::delete),
        )
        // Writes.
        .route(
            "/api/servers/{configId}/vs/{sid}/clients/{clid}/kick",
            post(clients::kick),
        )
        .route(
            "/api/servers/{configId}/vs/{sid}/clients/{clid}/mute",
            post(clients::mute),
        )
        .route(
            "/api/servers/{configId}/vs/{sid}/clients/{clid}/unmute",
            post(clients::unmute),
        )
        .route(
            "/api/servers/{configId}/vs/{sid}/clients/{clid}/move",
            post(clients::move_to),
        )
}

/// `{ "error": ..., "details"?: ..., "code"?: ... }` — spec §7.0.2 wire
/// shape. Mirrors [`crate::webquery::dashboard::ErrorBody`] but kept
/// module-local so we can extend it without churning the dashboard route.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ErrorBody {
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

pub(crate) fn err_body(status: StatusCode, body: ErrorBody) -> Response {
    (status, Json(body)).into_response()
}

pub(crate) fn err(status: StatusCode, message: &str) -> Response {
    err_body(
        status,
        ErrorBody {
            error: message.to_string(),
            code: None,
            details: None,
        },
    )
}

pub(crate) fn not_found() -> Response {
    err(StatusCode::NOT_FOUND, "Not found")
}

pub(crate) fn internal() -> Response {
    err(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
}

pub(crate) fn bad_request(message: &str) -> Response {
    err(StatusCode::BAD_REQUEST, message)
}

/// §7.0.2 translation for `WebQueryError`. Single source of truth — every
/// control handler funnels upstream errors through this.
pub(crate) fn translate_webquery_error(e: WebQueryError) -> Response {
    let status = e.http_status();
    match status {
        StatusCode::BAD_GATEWAY => err_body(
            status,
            ErrorBody {
                error: "TeamSpeak API Error".into(),
                code: Some(e.upstream_code()),
                details: Some(e.upstream_message()),
            },
        ),
        _ => err_body(
            status,
            ErrorBody {
                error: "Internal server error".into(),
                code: None,
                details: None,
            },
        ),
    }
}
