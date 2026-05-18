//! `/music-bots` — CRUD + lifecycle + SSE event stream
//! (PURA-117 / PURA-123 WS-5).

use std::convert::Infallible;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use futures::stream::{Stream, StreamExt};
use music_bot::{
    BotCommand, BotConfig, BotEvent as DomainBotEvent, BotState as DomainBotState, BotSupervisor,
    DisconnectKind,
};
use tokio_stream::wrappers::BroadcastStream;
use tracing::{info, warn};
use ts6_manager_shared::music_bots as wire;

use crate::app_state::AppState;
use crate::auth::extractors::RequireAuth;
use crate::routes::music_bots::convert::{bot_id_to_wire, bot_state_to_wire, track_to_wire};
use crate::routes::music_bots::{not_found, translate_send_error, validation};

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/api/music-bots", get(list).post(create))
        .route("/api/music-bots/{id}", get(detail).delete(shutdown))
        .route("/api/music-bots/{id}/connect", post(connect))
        .route("/api/music-bots/{id}/disconnect", post(disconnect))
        .route("/api/music-bots/{id}/join", post(join))
        .route("/api/music-bots/{id}/leave", post(leave))
        .route("/api/music-bots/{id}/events", get(events_sse))
}

async fn list(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
) -> Result<Json<Vec<wire::MusicBotSummary>>, Response> {
    let supervisor = &state.music_bots.supervisor;
    let infos = supervisor.list().await;
    let mut out = Vec::with_capacity(infos.len());
    for info in infos {
        let liveness = state.music_bots.liveness.snapshot(info.id).await;
        out.push(wire::MusicBotSummary {
            id: bot_id_to_wire(info.id),
            name: info.name,
            server_addr: info.server_addr,
            state: bot_state_to_wire(liveness.state, liveness.now_playing.is_some()),
            now_playing: liveness.now_playing.as_ref().map(track_to_wire),
            now_playing_elapsed_secs: liveness
                .now_playing
                .as_ref()
                .and(liveness.now_playing_elapsed_secs),
            last_error: liveness.last_error.clone(),
        });
    }
    out.sort_by_key(|b| b.id);
    Ok(Json(out))
}

async fn detail(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
) -> Result<Json<wire::MusicBotDetail>, Response> {
    let bot = music_bot::BotId(id);
    // Look the bot up in the supervisor's list — `subscribe` returns
    // `None` for a bot that was never spawned, but we want to 404 on a
    // detail GET specifically.
    let infos = state.music_bots.supervisor.list().await;
    let info = infos
        .into_iter()
        .find(|i| i.id == bot)
        .ok_or_else(|| not_found("bot not found"))?;
    let liveness = state.music_bots.liveness.snapshot(bot).await;
    let queue = state
        .music_bots
        .supervisor
        .store()
        .queue_peek(bot)
        .await
        .unwrap_or_default();
    Ok(Json(wire::MusicBotDetail {
        id: bot_id_to_wire(bot),
        name: info.name,
        server_addr: info.server_addr,
        state: bot_state_to_wire(liveness.state, liveness.now_playing.is_some()),
        now_playing: liveness.now_playing.as_ref().map(track_to_wire),
        now_playing_elapsed_secs: liveness
            .now_playing
            .as_ref()
            .and(liveness.now_playing_elapsed_secs),
        queue: queue.iter().map(track_to_wire).collect(),
        channel_id: liveness.channel_id,
        last_error: liveness.last_error.clone(),
    }))
}

async fn create(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Json(req): Json<wire::CreateBotRequest>,
) -> Result<(StatusCode, Json<wire::MusicBotSummary>), Response> {
    if req.name.trim().is_empty() {
        return Err(validation("name must not be empty"));
    }
    if req.server_addr.trim().is_empty() {
        return Err(validation("serverAddr must not be empty"));
    }

    // Allocate an id BEFORE deciding the identity path so the default
    // path embeds the (eventual) bot id and stays unique across
    // simultaneously-spawned bots without auto-rename ceremony.
    let supervisor: &BotSupervisor = &state.music_bots.supervisor;
    let identity_path = if let Some(p) = req.identity_path {
        std::path::PathBuf::from(p)
    } else {
        let dir = state.music_bots.identity_dir.as_ref();
        // Best-effort dir creation. If it fails (eg. permissions), the
        // first connect attempt surfaces the I/O error via
        // `BotEvent::Error::Connection` — no need to abort the create.
        let _ = std::fs::create_dir_all(dir);
        dir.join(format!(
            "bot-{}.identity",
            supervisor_next_hint(supervisor).await
        ))
    };

    let mut config = BotConfig::new(req.name.clone(), identity_path);
    config = config.with_server_addr(req.server_addr.clone());
    if let Some(auto) = req.auto_connect {
        config = config.with_auto_connect(auto);
    }

    let id = supervisor.spawn(config, state.yt_cookie.clone()).await;
    state.music_bots.watch(id).await;
    info!(bot = %id, name = %req.name, "music-bot created");

    let liveness = state.music_bots.liveness.snapshot(id).await;
    Ok((
        StatusCode::CREATED,
        Json(wire::MusicBotSummary {
            id: bot_id_to_wire(id),
            name: req.name,
            server_addr: req.server_addr,
            state: bot_state_to_wire(liveness.state, liveness.now_playing.is_some()),
            now_playing: liveness.now_playing.as_ref().map(track_to_wire),
            now_playing_elapsed_secs: liveness
                .now_playing
                .as_ref()
                .and(liveness.now_playing_elapsed_secs),
            last_error: liveness.last_error.clone(),
        }),
    ))
}

/// Best-effort hint at the next bot id the supervisor will mint. Used
/// only for the default identity-path filename — a tiny race here does
/// not corrupt anything (the supervisor still mints a unique id, the
/// identity file just lands at a slightly stale name).
async fn supervisor_next_hint(supervisor: &BotSupervisor) -> u64 {
    let infos = supervisor.list().await;
    infos.iter().map(|i| i.id.0).max().unwrap_or(0) + 1
}

async fn shutdown(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
) -> Result<StatusCode, Response> {
    let bot = music_bot::BotId(id);
    state
        .music_bots
        .supervisor
        .shutdown_bot(bot)
        .await
        .map_err(translate_send_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn connect(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
) -> Result<StatusCode, Response> {
    state
        .music_bots
        .supervisor
        .send(music_bot::BotId(id), BotCommand::Connect)
        .await
        .map_err(translate_send_error)?;
    Ok(StatusCode::ACCEPTED)
}

async fn disconnect(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
) -> Result<StatusCode, Response> {
    state
        .music_bots
        .supervisor
        .send(music_bot::BotId(id), BotCommand::Disconnect)
        .await
        .map_err(translate_send_error)?;
    Ok(StatusCode::ACCEPTED)
}

async fn join(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
    Json(req): Json<wire::JoinChannelRequest>,
) -> Result<StatusCode, Response> {
    state
        .music_bots
        .supervisor
        .send(
            music_bot::BotId(id),
            BotCommand::JoinChannel(req.channel_id),
        )
        .await
        .map_err(translate_send_error)?;
    Ok(StatusCode::ACCEPTED)
}

async fn leave(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
) -> Result<StatusCode, Response> {
    state
        .music_bots
        .supervisor
        .send(music_bot::BotId(id), BotCommand::LeaveChannel)
        .await
        .map_err(translate_send_error)?;
    Ok(StatusCode::ACCEPTED)
}

async fn events_sse(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Response> {
    let bot = music_bot::BotId(id);
    let rx = state
        .music_bots
        .supervisor
        .subscribe(bot)
        .await
        .ok_or_else(|| not_found("bot not found"))?;
    let stream = BroadcastStream::new(rx).filter_map(|item| async move {
        match item {
            Ok(ev) => match wire_event(&ev) {
                Some(wire) => match serde_json::to_string(&wire) {
                    Ok(json) => Some(Ok(Event::default().data(json))),
                    Err(err) => {
                        warn!(?err, "failed to serialise BotEvent for SSE");
                        None
                    }
                },
                None => None,
            },
            // Lagged subscribers are dropped silently — the FE refetches
            // the full bot detail on lag.
            Err(_) => None,
        }
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

/// Map a `music_bot::BotEvent` onto the wire `BotEventWire` projection.
/// Returns `None` for variants that don't translate to a wire event
/// (currently a placeholder — every variant maps).
fn wire_event(ev: &DomainBotEvent) -> Option<wire::BotEventWire> {
    Some(match ev {
        DomainBotEvent::StateChanged { from, to } => wire::BotEventWire::StateChanged {
            from: state_simple(*from),
            to: state_simple(*to),
        },
        DomainBotEvent::Connected {
            client_id,
            default_channel,
        } => wire::BotEventWire::Connected {
            client_id: *client_id,
            default_channel: *default_channel,
        },
        DomainBotEvent::Disconnected { kind, reason } => wire::BotEventWire::Disconnected {
            kind: match kind {
                DisconnectKind::Clean => "clean".into(),
                DisconnectKind::Dropped => "dropped".into(),
                DisconnectKind::ShutdownRequested => "shutdown_requested".into(),
            },
            reason: reason.clone(),
        },
        DomainBotEvent::JoinedChannel { channel_id } => wire::BotEventWire::JoinedChannel {
            channel_id: *channel_id,
        },
        DomainBotEvent::LeftChannel => wire::BotEventWire::LeftChannel,
        DomainBotEvent::QueueChanged { len, current } => wire::BotEventWire::QueueChanged {
            len: *len,
            current: current.as_ref().map(track_to_wire),
        },
        DomainBotEvent::NowPlaying(track) => wire::BotEventWire::NowPlaying {
            track: track_to_wire(track),
        },
        DomainBotEvent::QueueEmpty => wire::BotEventWire::QueueEmpty,
        DomainBotEvent::AudioFinished { reason } => wire::BotEventWire::AudioFinished {
            reason: reason.clone(),
        },
        // PURA-347 — playback-progress tick forwarded verbatim; the FE
        // reduces it into the now-playing progress bar.
        DomainBotEvent::Progress { elapsed_secs } => wire::BotEventWire::Progress {
            elapsed_secs: *elapsed_secs,
        },
        DomainBotEvent::PlaylistChanged(name) => wire::BotEventWire::PlaylistChanged {
            name: name.0.clone(),
        },
        DomainBotEvent::LibraryChanged => wire::BotEventWire::LibraryChanged,
        DomainBotEvent::Error(err) => wire::BotEventWire::Error {
            message: err.to_string(),
        },
    })
}

/// `bot_state_to_wire` synthesises `Playing`; the SSE stream's
/// `StateChanged` event reports the underlying FSM transition verbatim
/// because subscribers wire their own derivation off `now_playing`. This
/// helper drops the `Playing` synthesis so the `from`/`to` pair always
/// reflects the FSM exactly.
fn state_simple(state: DomainBotState) -> wire::BotState {
    match state {
        DomainBotState::Disconnected => wire::BotState::Disconnected,
        DomainBotState::Connecting => wire::BotState::Connecting,
        DomainBotState::Connected => wire::BotState::Connected,
        DomainBotState::InChannel => wire::BotState::InChannel,
        DomainBotState::Disconnecting => wire::BotState::Disconnecting,
    }
}
