//! Music-bot REST surface (PURA-117 / PURA-123 WS-5).
//!
//! Mirrors the `/api/servers` style: absolute paths, `RequireAuth`
//! extractor on every endpoint, JSON-only request + response bodies, the
//! shared `ts6_manager_shared::music_bots::ErrorBody` envelope on every
//! non-2xx. Routes are split per resource for review legibility — the
//! `router()` constructor in this module is the single mount point.
//!
//! Resources:
//! - `bots`            — `/music-bots[/{id}/{...}]`, plus the `/events` SSE.
//! - `library`         — `/music-library`.
//! - `playlists`       — `/playlists[/{name}/...]`, query-scoped by `bot`.
//! - `radio_stations`  — `/radio-stations`, library entries marked with
//!                       the [`wire::RADIO_TAG`] tag.
//! - `requests`        — `/music-requests` log.
//!
//! Auth: a fresh `RequireAuth` lookup runs on every handler (per spec
//! §6.4.1). RBAC granularity is "any authenticated user" — multi-tenant
//! / per-bot ACLs are flagged for follow-up on the parent epic.

mod audio_control;
mod bots;
mod convert;
mod library;
mod playlists;
mod queue;
mod radio_stations;
mod requests;

#[cfg(test)]
mod tests;

use axum::Json;
use axum::Router;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use ts6_manager_shared::music_bots::ErrorBody;

use crate::app_state::AppState;

/// Build the music-bot sub-router. The caller `merge`s this into the
/// top-level router so the absolute paths line up exactly with the
/// `docs/voice/music-bots-api.md` table.
pub fn router() -> Router<AppState> {
    Router::new()
        .merge(bots::router())
        .merge(audio_control::router())
        .merge(queue::router())
        .merge(library::router())
        .merge(playlists::router())
        .merge(radio_stations::router())
        .merge(requests::router())
}

// ---- Error helpers (shared by every submodule) -------------------------

pub(super) fn err(status: StatusCode, message: &str) -> Response {
    (status, Json(ErrorBody::new(message))).into_response()
}

pub(super) fn err_with_code(status: StatusCode, message: &str, code: &str) -> Response {
    (status, Json(ErrorBody::new(message).with_code(code))).into_response()
}

pub(super) fn not_found(what: &str) -> Response {
    err_with_code(StatusCode::NOT_FOUND, what, "not_found")
}

pub(super) fn validation(message: &str) -> Response {
    err_with_code(StatusCode::BAD_REQUEST, message, "validation")
}

pub(super) fn conflict(message: &str) -> Response {
    err_with_code(StatusCode::CONFLICT, message, "conflict")
}

pub(super) fn internal(message: &str) -> Response {
    err(StatusCode::INTERNAL_SERVER_ERROR, message)
}

/// Translate a `music_bot::StoreError` into an `ErrorBody` response. Used
/// by every resource that touches the bot store directly.
pub(super) fn translate_store_error(err: music_bot::StoreError) -> Response {
    use music_bot::StoreError;
    match err {
        StoreError::PlaylistNotFound(_)
        | StoreError::TrackNotFound(_)
        | StoreError::LibraryEntryNotFound(_) => not_found(&err.to_string()),
        StoreError::PlaylistExists(_) => conflict(&err.to_string()),
        StoreError::ReorderMismatch { .. } => validation(&err.to_string()),
        StoreError::Snapshot(_) | StoreError::Backend(_) => internal(&err.to_string()),
    }
}

/// Translate a `music_bot::SendError` into an `ErrorBody` response —
/// emitted by every lifecycle endpoint that dispatches a `BotCommand`.
pub(super) fn translate_send_error(err: music_bot::SendError) -> Response {
    use music_bot::SendError;
    match err {
        SendError::ActorGone => not_found("bot not found"),
        SendError::Full => err_with_code(
            StatusCode::SERVICE_UNAVAILABLE,
            "bot command queue full",
            "queue_full",
        ),
    }
}
