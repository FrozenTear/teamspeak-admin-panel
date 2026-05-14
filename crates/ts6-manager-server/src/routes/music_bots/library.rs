//! `/music-library` — per-bot saved-source catalog (PURA-123 WS-5).
//!
//! Library entries live in the `MusicBotStore` and survive snapshots /
//! restarts (the WS-5 swap-in for SurrealDB persistence is queued under
//! the parent epic).
//!
//! All endpoints are scoped by the `bot` query param so a single
//! `/music-library` URI works regardless of how many bots the operator
//! is running. Mirrors the WS-3 trait surface 1:1.

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use serde::Deserialize;
use ts6_manager_shared::music_bots as wire;

use crate::app_state::AppState;
use crate::auth::extractors::RequireAuth;
use crate::routes::music_bots::convert::{
    bot_id_from_wire, library_entry_id_from_wire, library_entry_to_wire,
    new_library_entry_from_wire,
};
use crate::routes::music_bots::{not_found, translate_store_error, validation};

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/api/music-library", get(list).post(add))
        .route(
            "/api/music-library/{trackId}",
            axum::routing::patch(patch).delete(remove),
        )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LibraryQuery {
    bot: wire::BotId,
    /// Optional tag filter — exact match.
    #[serde(default)]
    tag: Option<String>,
}

async fn list(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Query(q): Query<LibraryQuery>,
) -> Result<Json<Vec<wire::LibraryEntry>>, Response> {
    let entries = state
        .music_bots
        .supervisor
        .library_list(bot_id_from_wire(q.bot), q.tag.as_deref())
        .await
        .map_err(translate_store_error)?;
    Ok(Json(entries.iter().map(library_entry_to_wire).collect()))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddBody {
    bot: wire::BotId,
    #[serde(flatten)]
    entry: wire::AddLibraryEntryRequest,
}

async fn add(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Json(body): Json<AddBody>,
) -> Result<(StatusCode, Json<wire::LibraryEntry>), Response> {
    if body.entry.title.trim().is_empty() {
        return Err(validation("title must not be empty"));
    }
    let stored = state
        .music_bots
        .supervisor
        .library_add(
            bot_id_from_wire(body.bot),
            new_library_entry_from_wire(body.entry),
        )
        .await
        .map_err(translate_store_error)?;
    Ok((StatusCode::CREATED, Json(library_entry_to_wire(&stored))))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PatchBody {
    bot: wire::BotId,
    #[serde(flatten)]
    patch: wire::PatchLibraryEntryRequest,
}

async fn patch(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(track_id): Path<u64>,
    Json(body): Json<PatchBody>,
) -> Result<Json<wire::LibraryEntry>, Response> {
    let bot = bot_id_from_wire(body.bot);
    let entry_id = library_entry_id_from_wire(wire::LibraryEntryId(track_id));
    // Read-modify-write: WS-3's trait surface only exposes `add` /
    // `remove` / `lookup` / `list`. Replacing with a fresh entry under
    // the same id is intentionally NOT supported by the in-memory store
    // because it would surprise callers holding stale refs. Instead we
    // delete + re-add — the new entry gets a fresh id, which we return
    // so the caller can update its key.
    let existing = state
        .music_bots
        .supervisor
        .library_lookup(bot, entry_id)
        .await
        .map_err(translate_store_error)?
        .ok_or_else(|| not_found("library entry not found"))?;
    let removed = state
        .music_bots
        .supervisor
        .library_remove(bot, entry_id)
        .await
        .map_err(translate_store_error)?;
    if !removed {
        return Err(not_found("library entry not found"));
    }
    let updated = music_bot::NewLibraryEntry {
        source: existing.source,
        title: body.patch.title.unwrap_or(existing.title),
        tags: body.patch.tags.unwrap_or(existing.tags),
    };
    let fresh = state
        .music_bots
        .supervisor
        .library_add(bot, updated)
        .await
        .map_err(translate_store_error)?;
    Ok(Json(library_entry_to_wire(&fresh)))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteQuery {
    bot: wire::BotId,
}

async fn remove(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(track_id): Path<u64>,
    Query(q): Query<DeleteQuery>,
) -> Result<StatusCode, Response> {
    let removed = state
        .music_bots
        .supervisor
        .library_remove(
            bot_id_from_wire(q.bot),
            library_entry_id_from_wire(wire::LibraryEntryId(track_id)),
        )
        .await
        .map_err(translate_store_error)?;
    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(not_found("library entry not found"))
    }
}
