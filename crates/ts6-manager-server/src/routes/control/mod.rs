//! Phase 2 control surface — `/api/servers/{configId}/vs/{sid}/...` REST
//! endpoints (PURA-71).
//!
//! Each endpoint:
//! - Authenticates via the [`crate::auth::extractors::RequireAuth`] extractor
//!   chain (JWT + DB user lookup, spec §6.4.1). Inside the handler we run an
//!   additional per-server access check via [`access::check_read`] / [`access::check_write`].
//! - Resolves the `server_connection` row by `configId` and pulls an
//!   `Arc<dyn ControlBackend>` from [`crate::app_state::AppState::control`]
//!   (PURA-99). The pool branches on `controlPath` so kicks/moves/banadds
//!   dispatch over WebQuery HTTP or SSH ServerQuery transparently.
//! - Calls the typed [`crate::control::ControlBackend`] surface (read +
//!   write commands the FE consumes).
//! - On write success, emits a `tracing::info!` audit event under
//!   `target = "control::audit"` (see [`audit`]) AND publishes a
//!   `server:{configId}:clients` / `server:{configId}:channels` event on
//!   [`crate::app_state::AppState::ws_hub`] for live propagation.
//!
//! Errors map per spec §7.0.2 — see [`translate_control_error`]. The
//! envelope is identical for both backends because
//! [`crate::control::ControlBackendError`] is shape-aligned with
//! [`crate::webquery::WebQueryError`] / [`crate::sshbridge::SshBridgeError`].

use std::sync::Arc;
use std::time::Instant;

use axum::Json;
use axum::Router;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use serde::{Deserialize, Serialize};

use crate::app_state::AppState;
use crate::auth::extractors::AuthUser;
use crate::control::ControlBackendError;
use crate::repos::server_connections::ServerConnection;
use crate::webquery::{WebQueryClient, WebQueryError};
use crate::ws::topic::{Topic, TopicKind};

pub mod access;
pub mod audit;
pub mod bans;
pub mod channel_groups;
pub mod channels;
pub mod clients;
pub mod info;
pub mod logs;
pub mod messages;
pub mod permissions;
pub mod server_groups;
pub mod tokens;
pub mod video_sources;

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
        .route(
            "/api/servers/{configId}/vs/{sid}/bans",
            get(bans::list).post(bans::create),
        )
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
        // ---- PURA-373: server groups (spec §7.9) ----
        .route(
            "/api/servers/{configId}/vs/{sid}/server-groups",
            get(server_groups::list).post(server_groups::create),
        )
        .route(
            "/api/servers/{configId}/vs/{sid}/server-groups/{sgid}",
            put(server_groups::rename).delete(server_groups::delete),
        )
        .route(
            "/api/servers/{configId}/vs/{sid}/server-groups/{sgid}/copy",
            post(server_groups::copy),
        )
        .route(
            "/api/servers/{configId}/vs/{sid}/server-groups/{sgid}/members",
            get(server_groups::members).post(server_groups::add_member),
        )
        .route(
            "/api/servers/{configId}/vs/{sid}/server-groups/{sgid}/members/{cldbid}",
            delete(server_groups::remove_member),
        )
        .route(
            "/api/servers/{configId}/vs/{sid}/server-groups/{sgid}/permissions",
            get(server_groups::permissions)
                .put(server_groups::set_permission)
                .delete(server_groups::delete_permission),
        )
        // ---- PURA-373: channel groups (spec §7.10) ----
        .route(
            "/api/servers/{configId}/vs/{sid}/channel-groups",
            get(channel_groups::list).post(channel_groups::create),
        )
        .route(
            "/api/servers/{configId}/vs/{sid}/channel-groups/{cgid}",
            put(channel_groups::rename).delete(channel_groups::delete),
        )
        .route(
            "/api/servers/{configId}/vs/{sid}/channel-groups/{cgid}/clients",
            get(channel_groups::clients),
        )
        .route(
            "/api/servers/{configId}/vs/{sid}/channel-groups/{cgid}/assign",
            post(channel_groups::assign),
        )
        .route(
            "/api/servers/{configId}/vs/{sid}/channel-groups/{cgid}/permissions",
            get(channel_groups::permissions)
                .put(channel_groups::set_permission)
                .delete(channel_groups::delete_permission),
        )
        // ---- PURA-373: permissions catalog (spec §7.11, read-only) ----
        .route(
            "/api/servers/{configId}/vs/{sid}/permissions",
            get(permissions::list),
        )
        .route(
            "/api/servers/{configId}/vs/{sid}/permissions/find",
            get(permissions::find),
        )
        .route(
            "/api/servers/{configId}/vs/{sid}/permissions/overview/{cldbid}",
            get(permissions::overview),
        )
        // ---- PURA-373: tokens / privilege keys (spec §7.13) ----
        .route(
            "/api/servers/{configId}/vs/{sid}/tokens",
            get(tokens::list).post(tokens::create),
        )
        .route(
            "/api/servers/{configId}/vs/{sid}/tokens/{token}",
            delete(tokens::delete),
        )
        // ---- PURA-373: offline messages (spec §7.16) ----
        .route(
            "/api/servers/{configId}/vs/{sid}/messages",
            get(messages::list).post(messages::create),
        )
        .route(
            "/api/servers/{configId}/vs/{sid}/messages/{msgid}",
            get(messages::detail).delete(messages::delete),
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

/// §7.0.2 translation for [`ControlBackendError`]. Single source of
/// truth — every control handler funnels upstream errors through this.
/// Both WebQuery and SSH backends produce this same envelope because
/// [`ControlBackendError`] is shape-aligned with each backend's typed
/// error.
pub(crate) fn translate_control_error(e: ControlBackendError) -> Response {
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

/// §7.0.2 translation for a [`WebQueryError`]. The PURA-373 moderation
/// surfaces dispatch over WebQuery directly (pure passthrough — no SSH),
/// so they funnel errors through here rather than [`translate_control_error`].
/// [`WebQueryError`] converts into [`ControlBackendError`] losslessly, so
/// the envelope is identical to the rest of the control surface.
pub(crate) fn translate_webquery_error(e: WebQueryError) -> Response {
    translate_control_error(e.into())
}

/// Resolve the [`WebQueryClient`] for an already-access-checked
/// connection. Pool miss builds one from the row; build failure (apiKey
/// decrypt, bad header) maps through the §7.0.2 envelope.
pub(crate) async fn webquery_client(
    state: &AppState,
    connection: &ServerConnection,
) -> Result<Arc<WebQueryClient>, Response> {
    state
        .webquery
        .get_or_build(connection.id, Some(connection))
        .await
        .map_err(translate_webquery_error)
}

/// Emit a success audit entry for a moderation write (PURA-373). Mirrors
/// the [`bans`] handler's inline `AuditEntry::success` call so both layers
/// log under the same `control::audit` target.
pub(crate) fn audit_ok(
    connection_id: i64,
    sid: i64,
    user: &AuthUser,
    action: &'static str,
    target_id: Option<i64>,
    details: &str,
    started: Instant,
) {
    audit::AuditEntry::success(
        connection_id,
        sid,
        user.id,
        &user.username,
        action,
        target_id,
        details,
        started.elapsed(),
    )
    .emit();
}

/// Emit a failure audit entry for a moderation write and translate the
/// error into the §7.0.2 response. The WebQuery twin of the [`bans`]
/// module's `emit_failure`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_webquery_failure(
    user: &AuthUser,
    connection_id: i64,
    sid: i64,
    action: &'static str,
    target_id: Option<i64>,
    details: &str,
    err: WebQueryError,
    started: Instant,
) -> Response {
    let elapsed = started.elapsed();
    let entry = match &err {
        WebQueryError::Upstream { code, message } => audit::AuditEntry::upstream_error(
            connection_id,
            sid,
            user.id,
            &user.username,
            action,
            target_id,
            details,
            *code,
            message.clone(),
            elapsed,
        ),
        other => audit::AuditEntry::transport(
            connection_id,
            sid,
            user.id,
            &user.username,
            action,
            target_id,
            details,
            other.to_string(),
            elapsed,
        ),
    };
    entry.emit();
    translate_webquery_error(err)
}

/// Publish a moderation lifecycle event on the per-server `moderation`
/// topic (PURA-373). Server-group / channel-group / token / message
/// mutations fan out here so an open editor on another session refreshes.
pub(crate) async fn publish_moderation(
    state: &AppState,
    config_id: i64,
    kind: &'static str,
    data: serde_json::Value,
) {
    let topic = Topic::new(config_id, TopicKind::Moderation);
    let _ = state.ws_hub.publish(topic, kind, data).await;
}
