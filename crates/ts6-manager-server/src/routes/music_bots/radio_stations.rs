//! `/radio-stations` — saved radio-source presets (PURA-123 WS-5).
//!
//! Backed by the per-bot library — each radio station is a
//! `LibraryEntry` that carries the [`wire::RADIO_TAG`] tag. Listing,
//! creating, and deleting goes through the same `MusicBotStore` as
//! `/music-library`, so the two surfaces never disagree.

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{delete, get, post};
use chrono::Utc;
use music_bot::{AudioCommand, BotCommand, NewLibraryEntry};
use serde::Deserialize;
use ts6_manager_shared::music_bots as wire;

use crate::app_state::AppState;
use crate::auth::extractors::RequireAuth;
use crate::routes::music_bots::convert::{
    audio_source_from_wire, audio_source_to_wire, bot_id_from_wire, library_entry_id_from_wire,
    radio_station_to_wire,
};
use crate::routes::music_bots::{
    not_found, translate_send_error, translate_store_error, validation,
};

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/api/radio-stations", get(list).post(create))
        .route("/api/radio-stations/{id}", delete(remove))
        .route("/api/radio-stations/{id}/play", post(play))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BotQuery {
    bot: wire::BotId,
}

async fn list(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Query(q): Query<BotQuery>,
) -> Result<Json<Vec<wire::RadioStation>>, Response> {
    let entries = state
        .music_bots
        .supervisor
        .library_list(bot_id_from_wire(q.bot), Some(wire::RADIO_TAG))
        .await
        .map_err(translate_store_error)?;
    Ok(Json(entries.iter().map(radio_station_to_wire).collect()))
}

async fn create(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Json(req): Json<wire::CreateRadioStationRequest>,
) -> Result<(StatusCode, Json<wire::RadioStation>), Response> {
    if req.title.trim().is_empty() {
        return Err(validation("title must not be empty"));
    }
    let bot = bot_id_from_wire(req.bot);
    // Build the tag set: server always adds RADIO_TAG; extras get
    // deduped while preserving call-order.
    let mut tags = vec![wire::RADIO_TAG.to_string()];
    for t in req.tags {
        if !tags.contains(&t) {
            tags.push(t);
        }
    }
    let entry = NewLibraryEntry {
        source: audio_source_from_wire(req.source),
        title: req.title,
        tags,
    };
    let stored = state
        .music_bots
        .supervisor
        .library_add(bot, entry)
        .await
        .map_err(translate_store_error)?;
    Ok((StatusCode::CREATED, Json(radio_station_to_wire(&stored))))
}

async fn remove(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
    Query(q): Query<BotQuery>,
) -> Result<StatusCode, Response> {
    let bot = bot_id_from_wire(q.bot);
    let entry_id = library_entry_id_from_wire(wire::LibraryEntryId(id));
    // Confirm the row carries RADIO_TAG before deleting — DELETE on a
    // non-radio library entry must 404 from the radio surface so the
    // operator can't accidentally rip a track out via the wrong path.
    let existing = state
        .music_bots
        .supervisor
        .library_lookup(bot, entry_id)
        .await
        .map_err(translate_store_error)?
        .ok_or_else(|| not_found("radio station not found"))?;
    if !existing.tags.iter().any(|t| t == wire::RADIO_TAG) {
        return Err(not_found("radio station not found"));
    }
    let removed = state
        .music_bots
        .supervisor
        .library_remove(bot, entry_id)
        .await
        .map_err(translate_store_error)?;
    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(not_found("radio station not found"))
    }
}

/// `POST /radio-stations/{id}/play?bot={botId}` — convenience that
/// dispatches `AudioCommand::Play` against the radio station's source
/// without going through the queue. WS-2's audio pipeline picks the
/// command up; the request log gets a row with `track_id: None` because
/// no queue track was minted.
async fn play(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
    Query(q): Query<BotQuery>,
) -> Result<StatusCode, Response> {
    let bot = bot_id_from_wire(q.bot);
    let entry = state
        .music_bots
        .supervisor
        .library_lookup(bot, library_entry_id_from_wire(wire::LibraryEntryId(id)))
        .await
        .map_err(translate_store_error)?
        .ok_or_else(|| not_found("radio station not found"))?;
    if !entry.tags.iter().any(|t| t == wire::RADIO_TAG) {
        return Err(not_found("radio station not found"));
    }
    state
        .music_bots
        .supervisor
        .send(
            bot,
            BotCommand::Audio(AudioCommand::Play {
                source: entry.source.clone(),
            }),
        )
        .await
        .map_err(translate_send_error)?;
    state
        .music_bots
        .requests
        .record(wire::MusicRequest {
            id: 0,
            bot: q.bot,
            track_id: None,
            source: audio_source_to_wire(&entry.source),
            title: entry.title.clone(),
            requested_by: None,
            requested_at: Utc::now(),
        })
        .await;
    Ok(StatusCode::ACCEPTED)
}
