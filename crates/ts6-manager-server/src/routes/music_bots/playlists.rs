//! `/playlists` — per-bot playlist CRUD + enqueue (PURA-123 WS-5).

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{delete, get, post};
use chrono::Utc;
use music_bot::{BotCommand, PlaylistName, QueueCommand};
use serde::Deserialize;
use ts6_manager_shared::music_bots as wire;

use crate::app_state::AppState;
use crate::auth::extractors::RequireAuth;
use crate::routes::music_bots::convert::{
    audio_source_to_wire, bot_id_from_wire, new_track_from_wire, playlist_name_from_str,
    track_id_from_wire, track_to_wire,
};
use crate::routes::music_bots::{not_found, translate_send_error, translate_store_error, validation};

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/api/playlists", get(list).post(create))
        .route(
            "/api/playlists/{name}",
            get(detail).patch(rename).delete(remove),
        )
        .route(
            "/api/playlists/{name}/tracks",
            post(add_track),
        )
        .route(
            "/api/playlists/{name}/tracks/{trackId}",
            delete(remove_track),
        )
        .route(
            "/api/playlists/{name}/enqueue",
            post(enqueue),
        )
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
) -> Result<Json<Vec<wire::PlaylistSummary>>, Response> {
    let bot = bot_id_from_wire(q.bot);
    let names = state
        .music_bots
        .supervisor
        .playlist_list(bot)
        .await
        .map_err(translate_store_error)?;
    let mut summaries = Vec::with_capacity(names.len());
    for name in names {
        // Track count via a list — fine for the WS-5 in-memory impl;
        // the SurrealDB swap can replace this with a single COUNT query.
        let tracks = state
            .music_bots
            .supervisor
            .playlist_list_tracks(bot, &name)
            .await
            .map_err(translate_store_error)?;
        summaries.push(wire::PlaylistSummary {
            bot: q.bot,
            name: name.0,
            track_count: tracks.len(),
        });
    }
    Ok(Json(summaries))
}

async fn create(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Json(req): Json<wire::CreatePlaylistRequest>,
) -> Result<(StatusCode, Json<wire::PlaylistSummary>), Response> {
    if req.name.trim().is_empty() {
        return Err(validation("name must not be empty"));
    }
    let bot = bot_id_from_wire(req.bot);
    state
        .music_bots
        .supervisor
        .playlist_create(bot, PlaylistName(req.name.clone()))
        .await
        .map_err(translate_store_error)?;
    Ok((
        StatusCode::CREATED,
        Json(wire::PlaylistSummary {
            bot: req.bot,
            name: req.name,
            track_count: 0,
        }),
    ))
}

async fn detail(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(name): Path<String>,
    Query(q): Query<BotQuery>,
) -> Result<Json<wire::PlaylistDetail>, Response> {
    let bot = bot_id_from_wire(q.bot);
    let pn = playlist_name_from_str(&name);
    let tracks = state
        .music_bots
        .supervisor
        .playlist_list_tracks(bot, &pn)
        .await
        .map_err(translate_store_error)?;
    Ok(Json(wire::PlaylistDetail {
        bot: q.bot,
        name,
        tracks: tracks.iter().map(track_to_wire).collect(),
    }))
}

async fn rename(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(name): Path<String>,
    Query(q): Query<BotQuery>,
    Json(body): Json<wire::PatchPlaylistRequest>,
) -> Result<Json<wire::PlaylistSummary>, Response> {
    let new_name = body
        .new_name
        .ok_or_else(|| validation("newName must be provided"))?;
    if new_name.trim().is_empty() {
        return Err(validation("newName must not be empty"));
    }
    let bot = bot_id_from_wire(q.bot);
    state
        .music_bots
        .supervisor
        .playlist_rename(bot, PlaylistName(name.clone()), PlaylistName(new_name.clone()))
        .await
        .map_err(translate_store_error)?;
    let tracks = state
        .music_bots
        .supervisor
        .playlist_list_tracks(bot, &PlaylistName(new_name.clone()))
        .await
        .map_err(translate_store_error)?;
    Ok(Json(wire::PlaylistSummary {
        bot: q.bot,
        name: new_name,
        track_count: tracks.len(),
    }))
}

async fn remove(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(name): Path<String>,
    Query(q): Query<BotQuery>,
) -> Result<StatusCode, Response> {
    let bot = bot_id_from_wire(q.bot);
    state
        .music_bots
        .supervisor
        .playlist_delete(bot, PlaylistName(name))
        .await
        .map_err(translate_store_error)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddTrackBody {
    bot: wire::BotId,
    #[serde(flatten)]
    track: wire::AddTrackRequest,
}

async fn add_track(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(name): Path<String>,
    Json(body): Json<AddTrackBody>,
) -> Result<(StatusCode, Json<wire::Track>), Response> {
    if body.track.title.trim().is_empty() {
        return Err(validation("title must not be empty"));
    }
    let bot = bot_id_from_wire(body.bot);
    let pn = PlaylistName(name);
    let stored = state
        .music_bots
        .supervisor
        .playlist_add_track(bot, &pn, new_track_from_wire(body.track))
        .await
        .map_err(translate_store_error)?;
    Ok((StatusCode::CREATED, Json(track_to_wire(&stored))))
}

async fn remove_track(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path((name, track_id)): Path<(String, u64)>,
    Query(q): Query<BotQuery>,
) -> Result<StatusCode, Response> {
    let bot = bot_id_from_wire(q.bot);
    let pn = PlaylistName(name);
    let removed = state
        .music_bots
        .supervisor
        .playlist_remove_track(bot, &pn, track_id_from_wire(wire::TrackId(track_id)))
        .await
        .map_err(translate_store_error)?;
    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(not_found("track not in playlist"))
    }
}

/// `POST /playlists/{name}/enqueue?bot={id}` — append every track in the
/// playlist to the bot's queue. The dispatch goes through
/// `BotCommand::Queue(EnqueuePlaylist)` so the bot actor's
/// QueueChanged / NowPlaying events fire on the broadcast channel; the
/// store mutation is what the audio task picks up on next dequeue.
async fn enqueue(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(name): Path<String>,
    Query(q): Query<BotQuery>,
) -> Result<Json<wire::PlaylistDetail>, Response> {
    let bot = bot_id_from_wire(q.bot);
    let pn = PlaylistName(name.clone());
    // Resolve track list before the dispatch so we can populate the
    // request log + return the wire payload to the caller.
    let tracks = state
        .music_bots
        .supervisor
        .playlist_list_tracks(bot, &pn)
        .await
        .map_err(translate_store_error)?;
    state
        .music_bots
        .supervisor
        .send(bot, BotCommand::Queue(QueueCommand::EnqueuePlaylist(pn)))
        .await
        .map_err(translate_send_error)?;
    // Side-effect: log a request row per track so the operator can
    // audit "what got enqueued from playlist X".
    let now = Utc::now();
    for track in &tracks {
        state
            .music_bots
            .requests
            .record(wire::MusicRequest {
                id: 0, // record() stamps a fresh id
                bot: q.bot,
                track_id: Some(wire::TrackId(track.id.0)),
                source: audio_source_to_wire(&track.source),
                title: track.title.clone(),
                requested_by: track.requested_by.clone(),
                requested_at: now,
            })
            .await;
    }
    Ok(Json(wire::PlaylistDetail {
        bot: q.bot,
        name,
        tracks: tracks.iter().map(track_to_wire).collect(),
    }))
}
