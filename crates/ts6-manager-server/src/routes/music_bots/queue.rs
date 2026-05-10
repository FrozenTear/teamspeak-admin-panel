//! `/music-bots/{id}/queue[...]` — direct queue mutation surface
//! (PURA-126 WS-6 follow-up).
//!
//! Each route lowers to a `BotCommand::Queue(...)` dispatch through
//! `BotSupervisor::send`. The bot actor mutates the per-bot store on
//! receipt of the command (see `music_bot::bot::handle_queue_command`)
//! and emits the corresponding `QueueChanged` / `NowPlaying` /
//! `QueueEmpty` broadcast events for SSE subscribers. The `enqueue`
//! handler also records a `MusicRequest` row, mirroring the existing
//! `/playlists/{name}/enqueue` and `/radio-stations/{id}/play`
//! side-effects.

use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{delete, post};
use chrono::Utc;
use music_bot::{BotCommand, NewTrack, QueueCommand};
use ts6_manager_shared::music_bots as wire;

use crate::app_state::AppState;
use crate::auth::extractors::RequireAuth;
use crate::routes::music_bots::convert::{
    audio_source_from_wire, track_id_from_wire, track_to_wire,
};
use crate::routes::music_bots::translate_send_error;

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/api/music-bots/{id}/queue", post(enqueue).delete(clear))
        .route("/api/music-bots/{id}/queue/{trackId}", delete(remove))
        .route("/api/music-bots/{id}/queue/reorder", post(reorder))
        .route("/api/music-bots/{id}/queue/advance", post(advance))
}

async fn enqueue(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
    Json(req): Json<wire::EnqueueTrackRequest>,
) -> Result<StatusCode, Response> {
    let bot = music_bot::BotId(id);
    let domain_track = NewTrack {
        source: audio_source_from_wire(req.source.clone()),
        title: req.title.clone(),
        duration_secs: req.duration_secs,
        requested_by: req.requested_by.clone(),
    };
    state
        .music_bots
        .supervisor
        .send(bot, BotCommand::Queue(QueueCommand::Enqueue(domain_track)))
        .await
        .map_err(translate_send_error)?;
    // Side-effect: log a request row so the operator can audit "what
    // got enqueued via direct queue dispatch". Mirrors the playlist
    // enqueue handler. `track_id` stays `None` here because the actor
    // mints the queue id asynchronously — we don't try to round-trip it
    // back through the dispatch path (the SSE `QueueChanged` event
    // carries the freshly-stamped track for callers that need it).
    state
        .music_bots
        .requests
        .record(wire::MusicRequest {
            id: 0,
            bot: wire::BotId(id),
            track_id: None,
            source: req.source,
            title: req.title,
            requested_by: req.requested_by,
            requested_at: Utc::now(),
        })
        .await;
    Ok(StatusCode::ACCEPTED)
}

async fn clear(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
) -> Result<StatusCode, Response> {
    state
        .music_bots
        .supervisor
        .send(music_bot::BotId(id), BotCommand::Queue(QueueCommand::Clear))
        .await
        .map_err(translate_send_error)?;
    Ok(StatusCode::ACCEPTED)
}

async fn remove(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path((id, track_id)): Path<(u64, u64)>,
) -> Result<StatusCode, Response> {
    state
        .music_bots
        .supervisor
        .send(
            music_bot::BotId(id),
            BotCommand::Queue(QueueCommand::Remove(track_id_from_wire(wire::TrackId(
                track_id,
            )))),
        )
        .await
        .map_err(translate_send_error)?;
    Ok(StatusCode::ACCEPTED)
}

async fn reorder(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
    Json(req): Json<wire::ReorderQueueRequest>,
) -> Result<Json<Vec<wire::Track>>, Response> {
    let bot = music_bot::BotId(id);
    let domain_ids: Vec<music_bot::TrackId> =
        req.track_ids.into_iter().map(track_id_from_wire).collect();
    state
        .music_bots
        .supervisor
        .send(bot, BotCommand::Queue(QueueCommand::Reorder(domain_ids)))
        .await
        .map_err(translate_send_error)?;
    // Yield to let the actor drain the dispatched command before we
    // peek the store. The actor's mpsc consumer runs on the same
    // runtime and queue mutations are pure store ops (no I/O), so a
    // brief yield is enough for the snapshot to reflect the reorder.
    // SSE subscribers also see `QueueChanged` from the actor — that's
    // the authoritative live signal; this snapshot is a convenience
    // for the FE's optimistic rendering after a reorder gesture.
    tokio::time::sleep(Duration::from_millis(5)).await;
    let queue = state
        .music_bots
        .supervisor
        .store()
        .queue_peek(bot)
        .await
        .unwrap_or_default();
    Ok(Json(queue.iter().map(track_to_wire).collect()))
}

async fn advance(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
) -> Result<StatusCode, Response> {
    state
        .music_bots
        .supervisor
        .send(
            music_bot::BotId(id),
            BotCommand::Queue(QueueCommand::Advance),
        )
        .await
        .map_err(translate_send_error)?;
    Ok(StatusCode::ACCEPTED)
}
