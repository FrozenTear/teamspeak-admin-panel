//! `/music-bots/{id}/(play|pause|resume|stop|skip-next|skip-prev|volume)`
//! — audio control surface (PURA-126 WS-6 follow-up).
//!
//! Each route lowers to a `BotCommand::Audio(...)` dispatch via
//! `BotSupervisor::send`. `play` additionally writes a `MusicRequest`
//! row to the request log so the FE's "recently requested" widget shows
//! direct-source plays alongside playlist enqueues + radio plays.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::post;
use chrono::Utc;
use music_bot::{AudioCommand, BotCommand};
use ts6_manager_shared::music_bots as wire;

use crate::app_state::AppState;
use crate::auth::extractors::RequireAuth;
use crate::routes::music_bots::convert::audio_source_from_wire;
use crate::routes::music_bots::translate_send_error;

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/api/music-bots/{id}/play", post(play))
        .route("/api/music-bots/{id}/pause", post(pause))
        .route("/api/music-bots/{id}/resume", post(resume))
        .route("/api/music-bots/{id}/stop", post(stop))
        .route("/api/music-bots/{id}/skip-next", post(skip_next))
        .route("/api/music-bots/{id}/skip-prev", post(skip_prev))
        .route("/api/music-bots/{id}/volume", post(volume))
        .route("/api/music-bots/{id}/seek", post(seek))
}

async fn play(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
    Json(req): Json<wire::PlayRequest>,
) -> Result<StatusCode, Response> {
    let bot = music_bot::BotId(id);
    let domain_source = audio_source_from_wire(req.source.clone());
    state
        .music_bots
        .supervisor
        .send(
            bot,
            BotCommand::Audio(AudioCommand::Play {
                source: domain_source,
            }),
        )
        .await
        .map_err(translate_send_error)?;
    // Side-effect: record a MusicRequest row mirroring the
    // `/radio-stations/{id}/play` handler. `track_id` is `None` because
    // the play bypasses the queue. Title falls back to the source string
    // when the caller didn't supply one (no body field for it).
    let title = source_label(&req.source);
    state
        .music_bots
        .requests
        .record(wire::MusicRequest {
            id: 0,
            bot: wire::BotId(id),
            track_id: None,
            source: req.source,
            title,
            requested_by: None,
            requested_at: Utc::now(),
        })
        .await;
    Ok(StatusCode::ACCEPTED)
}

async fn pause(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
) -> Result<StatusCode, Response> {
    dispatch_audio(state, id, AudioCommand::Pause).await
}

async fn resume(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
) -> Result<StatusCode, Response> {
    dispatch_audio(state, id, AudioCommand::Resume).await
}

async fn stop(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
) -> Result<StatusCode, Response> {
    dispatch_audio(state, id, AudioCommand::Stop).await
}

async fn skip_next(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
) -> Result<StatusCode, Response> {
    dispatch_audio(state, id, AudioCommand::SkipNext).await
}

async fn skip_prev(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
) -> Result<StatusCode, Response> {
    dispatch_audio(state, id, AudioCommand::SkipPrev).await
}

async fn volume(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
    Json(req): Json<wire::SetVolumeRequest>,
) -> Result<StatusCode, Response> {
    dispatch_audio(state, id, AudioCommand::SetVolume(req.gain)).await
}

/// PURA-352 — scrub the current track to a position. Lowers to
/// `AudioCommand::Seek`; the bot re-spawns the decoder at the offset
/// reusing the already-resolved stream URL (no yt-dlp re-resolution).
async fn seek(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
    Json(req): Json<wire::SeekRequest>,
) -> Result<StatusCode, Response> {
    dispatch_audio(state, id, AudioCommand::Seek { secs: req.secs }).await
}

async fn dispatch_audio(
    state: AppState,
    id: u64,
    cmd: AudioCommand,
) -> Result<StatusCode, Response> {
    state
        .music_bots
        .supervisor
        .send(music_bot::BotId(id), BotCommand::Audio(cmd))
        .await
        .map_err(translate_send_error)?;
    Ok(StatusCode::ACCEPTED)
}

/// Best-effort title for a request-log row when the caller didn't
/// supply one (the `play` endpoint takes only `{ source }` — no title
/// field). Mirrors what an operator would see in the FE's request list:
/// the URL or library path is enough to identify the source.
fn source_label(source: &wire::AudioSource) -> String {
    match source {
        wire::AudioSource::Url { url } => url.clone(),
        wire::AudioSource::LibraryPath { path } => path.clone(),
    }
}
