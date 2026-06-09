//! Bot actor — PURA-118 WS-1 / PURA-154 audio integration.
//!
//! One actor task per bot. Owns the `Connection`, drives the lifecycle
//! state machine, dispatches `BotCommand`s, and emits `BotEvent`s onto a
//! broadcast channel.
//!
//! Audio dispatch (PURA-154) is wired through the [`crate::audio`]
//! sibling task: `BotCommand::Audio(Play { source })` spawns an
//! `AudioPipeline` (from `crates/music-bot-audio/`), the sibling task
//! forwards Opus 20 ms frames to this loop over an mpsc, and the
//! connected loop is the only thread that calls `Connection::send_audio`.
//! The borrow-checker dance ("events stream borrows `&mut con` for as
//! long as it lives; build the events future inline as the select arm so
//! it gets dropped each iteration") is the same one the WS-4 prototype
//! settled on — see `crates/ts6-voice-prototype/src/main.rs:152`.
//!
//! Cleanroom rule applies: this file derives the bot loop from the
//! `tsclientlib` upstream API and the existing `ts6-voice-prototype`
//! event-handling pattern only.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;
use tracing::{debug, error, info, warn};
use tsclientlib::{
    ChannelId as TsChannelId, ClientId, Connection, DisconnectOptions, MessageTarget, Reason,
    StreamItem, events::Event as BookEvent,
};
// `OutCommandExt::send` is the dispatch sink for any `Out…Part` message
// produced by the generated book→messages helpers (`client_move`, etc.).
// The prelude re-exports it as `_` so the methods light up via glob.
use tsclientlib::prelude::*;

use ts6_voice_fixture::{load_or_create_identity, wait_for_connected};

use music_bot_audio::{PipelineEvent, VolumeHandle};

use crate::audio::{self, ActiveAudio, AudioMsg};
use crate::backoff::ExponentialBackoff;
use crate::chat;
use crate::command::{AudioCommand, AudioSource, BotCommand, ChannelId, QueueCommand};
use crate::config::{BotConfig, BotId};
use crate::event::{BotError, BotEvent, DisconnectKind};
use crate::state::BotState;
use crate::store::{MusicBotStore, StoreError, Track, TrackId};

/// PURA-347 — frames per playback-progress tick. Opus frames carry 20 ms
/// of audio each, so 50 frames is exactly one second sent on the wire.
/// The connected loop emits a `BotEvent::Progress` every time
/// `frames_sent` crosses a multiple of this.
const FRAMES_PER_PROGRESS_TICK: u64 = 50;

/// Run the bot actor to completion. Exits when a `Shutdown` command has
/// been processed and the disconnect has flushed.
///
/// `yt_cookie` is a live-updated cookie-file path (PURA-223). The actor
/// reads the current value each time it starts a new yt-dlp pipeline so
/// a UI-uploaded cookie takes effect on the next track without a restart.
pub(crate) async fn run_bot(
    bot_id: BotId,
    config: BotConfig,
    store: Arc<dyn MusicBotStore>,
    mut rx: mpsc::Receiver<BotCommand>,
    events: broadcast::Sender<BotEvent>,
    yt_cookie: Arc<RwLock<Option<PathBuf>>>,
    yt_api_key: Arc<RwLock<Option<String>>>,
) {
    let span = tracing::info_span!("music_bot", bot_id = %bot_id, name = %config.name);
    let _enter = span.enter();
    info!("bot actor starting");

    let mut state = BotState::Disconnected;
    let mut backoff = ExponentialBackoff::new(config.backoff);
    // PURA-351 — the canonical output-gain handle. Owned by the actor so
    // an operator's volume setting survives reconnects (the connected loop
    // is re-entered on each handshake); cloned into every pipeline the bot
    // spawns so a change applies to the live track and every later one.
    let bot_volume = VolumeHandle::default();
    // Re-armed on every successful handshake; consumed by the connected loop.
    let mut connection: Option<Connection> = None;
    let mut shutdown_requested = false;

    // PURA-396 — pick the connected-loop implementation once, up front. The
    // config flag is `OR`-ed with the `VOICE_SPLIT_WIRE_TASK` env var so a
    // contabo-dev A/B is a pod env flip with no DB write. Default is the
    // single-loop path; `true` selects the PURA-389 §2a/2b wire/control
    // split.
    let split_wire = config.voice_split_wire_task || env_split_wire_task();
    info!(
        split_wire,
        "PURA-396 — connected-loop mode selected (split wire/control task = {split_wire})",
    );

    if config.auto_connect {
        debug!("auto_connect=true — queuing initial Connect");
    }

    'outer: loop {
        match state {
            BotState::Disconnected => {
                if shutdown_requested {
                    info!("shutdown done — actor exiting");
                    break 'outer;
                }
                let trigger = if config.auto_connect && backoff.attempts() == 0 {
                    Some(BotCommand::Connect)
                } else {
                    rx.recv().await
                };
                let Some(cmd) = trigger else {
                    info!("command channel closed — actor exiting");
                    break 'outer;
                };
                match cmd {
                    BotCommand::Connect => {
                        transition(&mut state, BotState::Connecting, &events);
                    }
                    BotCommand::Shutdown => {
                        shutdown_requested = true;
                        // Disconnected → Disconnecting isn't legal (and
                        // doesn't make sense — there's nothing to tear
                        // down). We loop back, hit the shutdown_requested
                        // gate above, and exit cleanly.
                        continue 'outer;
                    }
                    BotCommand::Disconnect => {
                        debug!("Disconnect ignored — already Disconnected");
                    }
                    BotCommand::Queue(qc) => {
                        // Queue ops are state-agnostic — staging a queue
                        // before connecting is a supported flow (chat
                        // bridge / REST in WS-4 / WS-5 will rely on it).
                        handle_queue_command(bot_id, &store, qc, &events).await;
                    }
                    other => emit_rejected(&events, &other, state),
                }
            }
            BotState::Connecting => {
                match attempt_connect(&config).await {
                    Ok((con, client_id, default_channel)) => {
                        backoff.reset();
                        transition(&mut state, BotState::Connected, &events);
                        let _ = events.send(BotEvent::Connected {
                            client_id: client_id.0,
                            default_channel,
                        });
                        let mut current_channel = Some(default_channel);
                        let _ = events.send(BotEvent::JoinedChannel {
                            channel_id: default_channel,
                        });
                        // Drive the connected loop until disconnected.
                        //
                        // PURA-396 — the split path moves `con` into the
                        // spawned wire task and clean-disconnects it there,
                        // so `con_after` comes back `None`; the single-loop
                        // path keeps `con` and disconnects it below.
                        let (outcome, mut con_after): (ConnectedExit, Option<Connection>) =
                            if split_wire {
                                let outcome = run_split_connected_loop(
                                    con,
                                    &mut state,
                                    &mut current_channel,
                                    &mut rx,
                                    &events,
                                    bot_id,
                                    &store,
                                    Arc::clone(&yt_cookie),
                                    Arc::clone(&yt_api_key),
                                    bot_volume.clone(),
                                )
                                .await;
                                (outcome, None)
                            } else {
                                let mut con = con;
                                let outcome = run_connected_loop(
                                    &mut con,
                                    &mut state,
                                    &mut current_channel,
                                    &mut rx,
                                    &events,
                                    bot_id,
                                    &store,
                                    Arc::clone(&yt_cookie),
                                    Arc::clone(&yt_api_key),
                                    bot_volume.clone(),
                                )
                                .await;
                                (outcome, Some(con))
                            };
                        match outcome {
                            ConnectedExit::Shutdown => {
                                shutdown_requested = true;
                                // Connected/InChannel → Disconnecting → Disconnected.
                                // The state machine rejects skipping
                                // Disconnecting; honour both transitions
                                // so the public event log is correct.
                                transition(&mut state, BotState::Disconnecting, &events);
                                if let Some(con) = con_after.as_mut() {
                                    clean_disconnect(con, "shutdown").await;
                                }
                                transition(&mut state, BotState::Disconnected, &events);
                                let _ = events.send(BotEvent::Disconnected {
                                    kind: DisconnectKind::ShutdownRequested,
                                    reason: "shutdown".into(),
                                });
                                connection = None;
                            }
                            ConnectedExit::Disconnect => {
                                transition(&mut state, BotState::Disconnecting, &events);
                                if let Some(con) = con_after.as_mut() {
                                    clean_disconnect(con, "disconnect").await;
                                }
                                transition(&mut state, BotState::Disconnected, &events);
                                let _ = events.send(BotEvent::Disconnected {
                                    kind: DisconnectKind::Clean,
                                    reason: "disconnect".into(),
                                });
                                connection = None;
                            }
                            ConnectedExit::Dropped(reason) => {
                                warn!(%reason, "connection dropped — auto-reconnect");
                                drop(con_after);
                                let _ = events.send(BotEvent::Disconnected {
                                    kind: DisconnectKind::Dropped,
                                    reason,
                                });
                                connection = None;
                                if let Some(delay) = backoff.next_delay() {
                                    info!(?delay, attempt = backoff.attempts(), "reconnect sleep");
                                    tokio::time::sleep(delay).await;
                                    // Stay in Connecting (legal self-loop).
                                    transition(&mut state, BotState::Connecting, &events);
                                } else {
                                    error!("max reconnect attempts reached — giving up");
                                    let _ = events.send(BotEvent::Error(BotError::Internal(
                                        "max reconnect attempts reached".into(),
                                    )));
                                    // Online → Disconnecting → Disconnected.
                                    transition(&mut state, BotState::Disconnecting, &events);
                                    transition(&mut state, BotState::Disconnected, &events);
                                }
                            }
                        }
                    }
                    Err(err) => {
                        error!(?err, "handshake failed");
                        let _ =
                            events.send(BotEvent::Error(BotError::Connection(format!("{err:#}"))));
                        if let Some(delay) = backoff.next_delay() {
                            info!(
                                ?delay,
                                attempt = backoff.attempts(),
                                "handshake retry sleep"
                            );
                            tokio::time::sleep(delay).await;
                            // Stay in Connecting.
                            continue 'outer;
                        } else {
                            error!("max handshake attempts reached — giving up");
                            transition(&mut state, BotState::Disconnected, &events);
                        }
                    }
                }
            }
            BotState::Connected | BotState::InChannel => {
                // Should not be observable here — the connected loop owns
                // these states and only returns after transitioning out.
                // Defensive break to avoid a busy loop if something goes
                // wrong.
                error!(?state, "unexpected state in outer loop — exiting");
                break 'outer;
            }
            BotState::Disconnecting => {
                // Outer loop reaches here only if the connected loop
                // returned without flipping us back to `Disconnected` —
                // shouldn't happen, but bail safely.
                transition(&mut state, BotState::Disconnected, &events);
            }
        }
    }

    if let Some(mut con) = connection.take() {
        clean_disconnect(&mut con, "actor exit").await;
    }
    info!("bot actor exited");
}

/// Outcome of the connected-loop. The outer state machine uses this to
/// decide whether to auto-reconnect or finish.
enum ConnectedExit {
    /// Caller asked for a clean disconnect (`Disconnect`).
    Disconnect,
    /// Caller asked for full shutdown (`Shutdown`).
    Shutdown,
    /// Stream errored / ended unexpectedly — auto-reconnect path.
    Dropped(String),
}

/// PURA-358 — a single `run_connected_loop` iteration that runs longer
/// than this starves the audio-drain arm. The audio sibling paces frames
/// on a 20 ms cadence; the `biased` select polls the audio arm first, but
/// once a *non-audio* arm body is executing nothing drains audio until it
/// returns. A body past this threshold means the next frame reaches the
/// wire late — the sporadic mid-song `frame_underrun` (with the frame
/// buffer still full, so consumer-side starvation) reported in PURA-358.
///
/// 10 ms sits below `audio::LATENESS_WARN` (12 ms) so a stalling handler
/// is logged *before* it tips into an audible crackle, yet well above the
/// sub-millisecond cost of normal event/command handling — so it flags a
/// real stall without false-positiving on routine iterations.
const LOOP_STALL_WARN: Duration = Duration::from_millis(10);

/// PURA-358 — emit a `connected_loop_stall` WARN when a select-arm body
/// outran [`LOOP_STALL_WARN`]. `detail` is only formatted when the stall
/// actually fired, so the hot path pays nothing on a healthy iteration.
fn log_loop_stall(arm: &'static str, arm_start: Instant, detail: impl FnOnce() -> String) {
    let elapsed = arm_start.elapsed();
    if elapsed >= LOOP_STALL_WARN {
        warn!(
            target: "music_bot_latency",
            stage = "connected_loop_stall",
            arm,
            detail = %detail(),
            elapsed_ms = elapsed.as_millis() as u64,
            "connected-loop arm body outran the 20 ms audio-frame cadence — the \
             audio-drain arm was starved this long; correlate with a mid-song \
             frame_underrun (buffered_frames full) to confirm it reached the wire",
        );
    }
}

/// Stable `&'static str` name for a [`BotCommand`] — used by
/// rejected-command logging and the connected-loop stall watchdog.
fn command_label(cmd: &BotCommand) -> &'static str {
    match cmd {
        BotCommand::Connect => "Connect",
        BotCommand::Disconnect => "Disconnect",
        BotCommand::JoinChannel(_) => "JoinChannel",
        BotCommand::LeaveChannel => "LeaveChannel",
        BotCommand::Shutdown => "Shutdown",
        BotCommand::Audio(_) => "Audio",
        BotCommand::Queue(_) => "Queue",
    }
}

/// PURA-358 — stable `&'static str` name for a [`StreamItem`] variant, so
/// the connected-loop stall watchdog can attribute a slow event-arm
/// iteration to the kind of protocol event that held the loop.
fn stream_item_label(item: &StreamItem) -> &'static str {
    match item {
        StreamItem::BookEvents(_) => "BookEvents",
        StreamItem::DisconnectedTemporarily(_) => "DisconnectedTemporarily",
        StreamItem::IdentityLevelIncreasing(_) => "IdentityLevelIncreasing",
        StreamItem::IdentityLevelIncreased => "IdentityLevelIncreased",
        _ => "other",
    }
}

/// Drive the event stream + command queue while the bot is online.
/// Mirrors `ts6-voice-prototype`'s borrow-checker dance: build the events
/// future inline as the select arm so it gets dropped at each iteration,
/// freeing `&mut con` for command dispatch / `send_audio` in the body.
#[allow(clippy::too_many_arguments)]
async fn run_connected_loop(
    con: &mut Connection,
    state: &mut BotState,
    current_channel: &mut Option<ChannelId>,
    rx: &mut mpsc::Receiver<BotCommand>,
    events: &broadcast::Sender<BotEvent>,
    bot_id: BotId,
    store: &Arc<dyn MusicBotStore>,
    yt_cookie: Arc<RwLock<Option<PathBuf>>>,
    yt_api_key: Arc<RwLock<Option<String>>>,
    bot_volume: VolumeHandle,
) -> ConnectedExit {
    // PURA-154 — `current_audio` is `Some` while a pipeline is spawned.
    // The connected loop is the sole owner: the actor's lifecycle owns
    // teardown (drop on shutdown / drop on reconnect), and Stop / Play
    // commands flip it in place.
    let mut current_audio: Option<ActiveAudio> = None;

    loop {
        tokio::select! {
            biased;
            // PURA-342 — audio frames first. The sibling paces frames at a
            // 20 ms cadence, so this arm becomes ready at most once per 20 ms;
            // the gap between frames still belongs to the event stream below.
            // But when a frame *is* due it must reach the wire before the next
            // protocol event is processed. The connect/book-sync handshake
            // streams a burst of TS6 events — exactly the startup window —
            // and with the event arm first that burst monopolised the loop:
            // audio frames piled in `audio_rx`, the sibling blocked on its
            // send, and the wire gapped (audible startup crackle, PURA-342).
            // The frame buffer is not underrunning when this happens — it
            // stays full; the connected loop simply wasn't polling this arm.
            audio_msg = async {
                // Unwrap is sound because the guard below gates entry;
                // `audio_rx` is `Some` for the whole life of `current_audio`
                // on this single-loop path (only the split path `take`s it).
                current_audio.as_mut().unwrap().audio_rx.as_mut().unwrap().recv().await
            }, if current_audio.is_some() => {
                // PURA-358 — time the audio-arm body. `send_audio`
                // contention on a `frame`, or an ~11 s yt-dlp auto-advance
                // on a `finished`, both stall the loop here.
                let arm_start = Instant::now();
                let kind = handle_audio_msg(
                    &mut WireSink::Direct(&mut *con),
                    audio_msg,
                    &mut current_audio,
                    bot_id,
                    store,
                    events,
                    &yt_cookie,
                    &bot_volume,
                ).await;
                log_loop_stall("audio", arm_start, || format!("audio_msg={kind}"));
            },
            ev = async { con.events().next().await } => match ev {
                Some(Ok(item)) => {
                    // PURA-358 — time the event-arm body. A heavy TS6
                    // event handler, or a chat command that runs a
                    // synchronous network round-trip, stalls the loop here.
                    let arm_start = Instant::now();
                    let item_label = stream_item_label(&item);
                    // PURA-122 WS-4 — pull any in-channel chat messages
                    // out of `BookEvents` BEFORE the channel-update logic
                    // consumes the item. Cheap because we only clone the
                    // event vector when chat is actually present.
                    let chat_msgs = extract_channel_chat(&item, con);
                    let chat_lines = chat_msgs.len();
                    if let Some(channel) = handle_stream_item(item, con)
                        && Some(channel) != *current_channel {
                            *current_channel = Some(channel);
                            transition(state, BotState::InChannel, events);
                            let _ = events.send(BotEvent::JoinedChannel { channel_id: channel });
                        }
                    for msg in chat_msgs {
                        // PURA-340 — `current_audio` + `yt_cookie` are
                        // threaded in so a queue-mutating chat command
                        // (`!play` etc.) can actually start the pipeline.
                        dispatch_chat_line(
                            &mut WireSink::Direct(&mut *con),
                            &mut current_audio,
                            bot_id,
                            store,
                            events,
                            &yt_cookie,
                            &yt_api_key,
                            &bot_volume,
                            &msg,
                        )
                        .await;
                    }
                    log_loop_stall("event", arm_start, || {
                        format!("item={item_label} chat_lines={chat_lines}")
                    });
                }
                Some(Err(err)) => {
                    return ConnectedExit::Dropped(format!("stream error: {err}"));
                }
                None => return ConnectedExit::Dropped("stream ended".into()),
            },
            cmd = rx.recv() => match cmd {
                Some(BotCommand::Disconnect) => {
                    if audio::tear_down(&mut current_audio) {
                        audio::send_voice_stop(con);
                        let _ = events.send(BotEvent::AudioFinished {
                            reason: "disconnect".into(),
                        });
                    }
                    return ConnectedExit::Disconnect;
                }
                Some(BotCommand::Shutdown) => {
                    if audio::tear_down(&mut current_audio) {
                        audio::send_voice_stop(con);
                        let _ = events.send(BotEvent::AudioFinished {
                            reason: "shutdown".into(),
                        });
                    }
                    return ConnectedExit::Shutdown;
                }
                Some(BotCommand::Connect) => {
                    debug!("Connect ignored — already online");
                }
                Some(BotCommand::JoinChannel(target)) => {
                    if let Err(err) = send_channel_move(con, target) {
                        let _ = events.send(BotEvent::Error(BotError::Connection(format!("{err:#}"))));
                    }
                    // The `JoinedChannel` event fires when the book event
                    // confirms the move — see handle_stream_item.
                }
                Some(BotCommand::LeaveChannel) => {
                    // Returning to the server's default channel = move
                    // back to channel id 0 (the server places us in
                    // `default_channel` per options). For WS-1 this is a
                    // best-effort no-op pending WS-3 channel hierarchy
                    // tracking; the event still fires for symmetry so
                    // callers can wire UI off it.
                    let _ = events.send(BotEvent::LeftChannel);
                    if let Some(id) = *current_channel {
                        debug!(channel_id = id, "LeaveChannel — staying in current channel until WS-3 default-channel tracking lands");
                    }
                }
                Some(BotCommand::Audio(audio_cmd)) => {
                    // PURA-358 — `AudioCommand::Play` / `Seek` resolve via
                    // yt-dlp (~11 s) inline on this loop; time the body so a
                    // command-driven stall is attributed, not just guessed.
                    let arm_start = Instant::now();
                    handle_audio_command(
                        &mut WireSink::Direct(&mut *con),
                        audio_cmd,
                        &mut current_audio,
                        bot_id,
                        store,
                        events,
                        &yt_cookie,
                        &bot_volume,
                    ).await;
                    log_loop_stall("command", arm_start, || "cmd=Audio".to_string());
                }
                Some(BotCommand::Queue(qc)) => {
                    // PURA-358 — queue mutations hit the store (DB round
                    // trips); time the body so a slow store call is logged.
                    let arm_start = Instant::now();
                    handle_queue_command(bot_id, store, qc, events).await;
                    log_loop_stall("command", arm_start, || "cmd=Queue".to_string());
                }
                None => return ConnectedExit::Dropped("command channel closed".into()),
            },
        }
    }
}

// ===========================================================================
// PURA-396 / PURA-389 §2a+2b — split wire/control connected loop.
//
// The single-loop `run_connected_loop` above polls audio, protocol events,
// and commands from one `select!`. PURA-389a measured that ~79 % of the
// residual `arm=audio` stall is candidate C — a paced frame waiting in the
// sibling→loop mpsc behind a busy event/command arm (book bursts, chat
// dispatch, an ~11 s yt-dlp resolve). The fix is to give the connection a
// dedicated **wire task** whose every loop iteration is structurally bounded
// to one frame / one event / one command, and move all the heavy work to a
// **control task**. `tsclientlib::Connection` is single-owner, so the wire
// task owns it outright and the control task reaches the wire only through
// the `WireCmd` channel.
//
// Gated behind `BotConfig::voice_split_wire_task` (default off) — the
// single-loop path above is the untouched rollback.
// ===========================================================================

/// PURA-396 — is the `VOICE_SPLIT_WIRE_TASK` env override set truthy? Lets a
/// contabo-dev A/B flip the split loop on per pod without a DB write.
fn env_split_wire_task() -> bool {
    split_flag_truthy(std::env::var("VOICE_SPLIT_WIRE_TASK").ok().as_deref())
}

/// PURA-396 — parse a truthy env-flag value: trimmed, case-insensitive on the
/// common spellings. Pure so it is unit-testable without touching the
/// process environment.
fn split_flag_truthy(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on"),
    )
}

/// PURA-396 — a command for the wire task: an operation that needs
/// `&mut Connection`. The control task produces these; the wire task is the
/// sole executor. A single FIFO `mpsc` keeps audio vs. voice-stop ordering:
/// `ClearAudio`/`VoiceStop` issued on teardown are dequeued in send order.
enum WireCmd {
    /// Install a freshly-spawned pipeline's frame receiver. `epoch` tags the
    /// pipeline generation so a stale teardown event from a since-replaced
    /// pipeline can be discarded by the control task.
    InstallAudio {
        rx: mpsc::Receiver<AudioMsg>,
        started_at: std::time::Instant,
        seek_base_secs: u64,
        epoch: u64,
    },
    /// Drop the installed frame receiver — discards any frames still buffered
    /// behind it (teardown with no replacement, e.g. `!stop`).
    ClearAudio,
    /// Empty-payload voice packet — flushes listener jitter buffers.
    VoiceStop,
    /// `clientmove` to a channel.
    ChannelMove(ChannelId),
    /// A channel-chat reply line.
    ChatReply(String),
    /// Clean-disconnect the connection and exit the wire task.
    Disconnect { shutdown: bool },
}

/// PURA-396 — an event from the wire task back to the control task. The
/// audio-lifecycle variants carry the pipeline-generation `epoch` so the
/// control task can ignore an event from a pipeline it has already replaced.
enum WireEvent {
    /// Own client's channel per the latest book update.
    Channel(ChannelId),
    /// Channel-chat lines extracted from a `BookEvents` item.
    Chat(Vec<ChatLine>),
    /// A pipeline out-of-band event (ICY metadata, warning, EOS).
    Pipeline(PipelineEvent),
    /// The sibling drained cleanly and sent `Finished`; `frames_sent` is the
    /// wire task's count, used for the 0-frame failure detection.
    AudioFinished { frames_sent: u64, epoch: u64 },
    /// The sibling channel closed without a `Finished` (crash path).
    SiblingClosed { epoch: u64 },
    /// `send_audio` returned an error for a frame.
    SendFailed { error: String, epoch: u64 },
    /// The connection event stream errored or ended.
    Dropped(String),
}

/// PURA-396 — the sink for the handful of operations that need
/// `&mut Connection`. The single-loop path wraps the connection directly; the
/// split path forwards a [`WireCmd`] to the wire task. One `WireSink` lets
/// `handle_audio_msg` / `handle_audio_command` / `dispatch_chat_line` /
/// `apply_chat_audio_action` serve both paths unchanged.
enum WireSink<'a> {
    Direct(&'a mut Connection),
    Split(&'a mpsc::UnboundedSender<WireCmd>),
}

impl WireSink<'_> {
    /// Send an empty-payload voice packet (jitter-buffer flush).
    fn voice_stop(&mut self) {
        match self {
            WireSink::Direct(con) => audio::send_voice_stop(con),
            WireSink::Split(tx) => {
                let _ = tx.send(WireCmd::VoiceStop);
            }
        }
    }

    /// Drop the wire task's installed frame receiver. No-op on the
    /// single-loop path — there the receiver lives inside `ActiveAudio` and
    /// is already gone with the `audio::tear_down` the caller just ran.
    fn clear_audio(&mut self) {
        if let WireSink::Split(tx) = self {
            let _ = tx.send(WireCmd::ClearAudio);
        }
    }

    /// `clientmove` to `target`.
    fn channel_move(&mut self, target: ChannelId) -> Result<()> {
        match self {
            WireSink::Direct(con) => send_channel_move(con, target),
            WireSink::Split(tx) => {
                let _ = tx.send(WireCmd::ChannelMove(target));
                Ok(())
            }
        }
    }

    /// Send a channel-chat reply line.
    fn chat_reply(&mut self, line: String) {
        match self {
            WireSink::Direct(con) => chat::send_reply(con, &line),
            WireSink::Split(tx) => {
                let _ = tx.send(WireCmd::ChatReply(line));
            }
        }
    }

    /// Send one paced Opus frame. Only the single-loop path reaches this — in
    /// the split path the wire task owns frame sends, so the control task
    /// never routes an `AudioMsg::Frame` through `handle_audio_msg`.
    #[allow(clippy::result_large_err)]
    fn send_opus_frame(
        &mut self,
        opus: &[u8],
        enqueued_at: std::time::Instant,
        monitor: &mut audio::SendTimingMonitor,
    ) -> Result<(), tsclientlib::Error> {
        match self {
            // Single-loop path keeps `block_in_place` (rollback fidelity).
            WireSink::Direct(con) => audio::send_opus_frame(con, opus, enqueued_at, monitor, true),
            WireSink::Split(_) => {
                unreachable!("split path: the wire task owns frame sends")
            }
        }
    }
}

/// PURA-396 — per-play frame-side state owned by the wire task. The
/// control-side counterpart stays in `ActiveAudio`.
struct WirePlay {
    /// Paced Opus frames + pipeline events from the audio sibling.
    rx: mpsc::Receiver<AudioMsg>,
    /// Frames sent on the wire so far (drives the progress tick).
    frames_sent: u64,
    /// Pipeline-spawn instant — `first_frame_on_wire` latency anchor.
    started_at: std::time::Instant,
    /// Playback offset this pipeline (re)started at (PURA-352 seek).
    seek_base_secs: u64,
    /// A/B/C send-path attribution accumulator (PURA-389a), per play.
    send_monitor: audio::SendTimingMonitor,
    /// Pipeline generation — echoed on the lifecycle `WireEvent`s.
    epoch: u64,
}

/// PURA-396 §2a — the **wire task**. Sole owner of `&mut Connection`. Its
/// `select!` has exactly three arms — a paced frame, a protocol event, a wire
/// command — and every arm body is bounded to one cheap operation, so the
/// 20 ms audio cadence is structurally isolated from the control task's chat
/// / queue / yt-dlp work. Consumes the `Connection`; clean-disconnects it on
/// a `WireCmd::Disconnect`.
async fn run_wire_task(
    mut con: Connection,
    mut wire_cmd_rx: mpsc::UnboundedReceiver<WireCmd>,
    wire_evt_tx: mpsc::UnboundedSender<WireEvent>,
    events: broadcast::Sender<BotEvent>,
) {
    let mut play: Option<WirePlay> = None;

    loop {
        tokio::select! {
            biased;
            // Paced audio frames first — the 20 ms cadence must win the race.
            msg = async { play.as_mut().unwrap().rx.recv().await }, if play.is_some() => {
                let arm_start = Instant::now();
                match msg {
                    Some(AudioMsg::Frame { bytes, enqueued_at }) => {
                        let p = play.as_mut().unwrap();
                        p.frames_sent += 1;
                        // PURA-330 — first audible frame; closes the
                        // `!play` → first-audio latency breakdown.
                        if p.frames_sent == 1 {
                            info!(
                                target: "music_bot_latency",
                                stage = "first_frame_on_wire",
                                elapsed_ms = p.started_at.elapsed().as_millis() as u64,
                                "first Opus frame sent on the wire — playback audible",
                            );
                            // THE-927 — clear any dashboard "Resolving
                            // YouTube…" pill the chat command lit up.
                            let _ = events.send(BotEvent::FirstFrameOnWire);
                        }
                        // PURA-347 — once-per-second playback-progress tick,
                        // offset by the PURA-352 seek base.
                        if p.frames_sent.is_multiple_of(FRAMES_PER_PROGRESS_TICK) {
                            let _ = events.send(BotEvent::Progress {
                                elapsed_secs: p.seek_base_secs
                                    + p.frames_sent / FRAMES_PER_PROGRESS_TICK,
                            });
                        }
                        // PURA-396 2b — `false`: no `block_in_place`. The
                        // wire task does nothing but wire I/O, so wrapping
                        // the microsecond send buys only churn (candidate A).
                        if let Err(err) = audio::send_opus_frame(
                            &mut con,
                            &bytes,
                            enqueued_at,
                            &mut p.send_monitor,
                            false,
                        ) {
                            error!(?err, "send_audio failed on the wire task");
                            let epoch = p.epoch;
                            play = None;
                            let _ = wire_evt_tx.send(WireEvent::SendFailed {
                                error: err.to_string(),
                                epoch,
                            });
                        }
                    }
                    Some(AudioMsg::PipelineEvent(ev)) => {
                        let _ = wire_evt_tx.send(WireEvent::Pipeline(ev));
                    }
                    Some(AudioMsg::Finished) => {
                        let p = play.take().expect("guard ensures Some");
                        let _ = wire_evt_tx.send(WireEvent::AudioFinished {
                            frames_sent: p.frames_sent,
                            epoch: p.epoch,
                        });
                    }
                    None => {
                        // Sibling channel closed without a `Finished`.
                        let epoch = play.take().expect("guard ensures Some").epoch;
                        let _ = wire_evt_tx.send(WireEvent::SiblingClosed { epoch });
                    }
                }
                // Should never trip — the body is one send / one channel
                // push. Logged so the A/B can prove the wire task is clean.
                log_loop_stall("wire_audio", arm_start, || "frame".to_string());
            },
            cmd = wire_cmd_rx.recv() => match cmd {
                Some(WireCmd::InstallAudio { rx, started_at, seek_base_secs, epoch }) => {
                    play = Some(WirePlay {
                        rx,
                        frames_sent: 0,
                        started_at,
                        seek_base_secs,
                        send_monitor: audio::SendTimingMonitor::new(),
                        epoch,
                    });
                }
                Some(WireCmd::ClearAudio) => {
                    play = None;
                }
                Some(WireCmd::VoiceStop) => audio::send_voice_stop(&mut con),
                Some(WireCmd::ChannelMove(target)) => {
                    if let Err(err) = send_channel_move(&mut con, target) {
                        let _ = events.send(BotEvent::Error(BotError::Connection(format!(
                            "{err:#}"
                        ))));
                    }
                }
                Some(WireCmd::ChatReply(line)) => chat::send_reply(&mut con, &line),
                Some(WireCmd::Disconnect { shutdown }) => {
                    clean_disconnect(&mut con, if shutdown { "shutdown" } else { "disconnect" })
                        .await;
                    return;
                }
                None => {
                    // Control task gone — exit without a clean disconnect
                    // (the actor is already tearing down).
                    return;
                }
            },
            ev = async { con.events().next().await } => {
                let arm_start = Instant::now();
                match ev {
                    Some(Ok(item)) => {
                        let item_label = stream_item_label(&item);
                        // Extract chat (borrows `&item`) before the
                        // channel-update logic consumes the item.
                        let chat_msgs = extract_channel_chat(&item, &con);
                        let chat_lines = chat_msgs.len();
                        if !chat_msgs.is_empty() {
                            let _ = wire_evt_tx.send(WireEvent::Chat(chat_msgs));
                        }
                        if let Some(channel) = handle_stream_item(item, &con) {
                            let _ = wire_evt_tx.send(WireEvent::Channel(channel));
                        }
                        log_loop_stall("wire_event", arm_start, || {
                            format!("item={item_label} chat_lines={chat_lines}")
                        });
                    }
                    Some(Err(err)) => {
                        let _ = wire_evt_tx.send(WireEvent::Dropped(format!("stream error: {err}")));
                        return;
                    }
                    None => {
                        let _ = wire_evt_tx.send(WireEvent::Dropped("stream ended".into()));
                        return;
                    }
                }
            },
        }
    }
}

/// PURA-396 — hand any freshly-spawned pipeline's frame receiver to the wire
/// task. Centralised at the top of the control loop so every spawn site
/// (direct `Play`, chat `!play`, queue auto-advance, seek) is covered by one
/// call. Bumps the pipeline-generation `epoch` so a later teardown event from
/// this generation is attributable.
fn install_pending_audio(
    current_audio: &mut Option<ActiveAudio>,
    epoch: &mut u64,
    wire_cmd_tx: &mpsc::UnboundedSender<WireCmd>,
) {
    let Some(active) = current_audio.as_mut() else {
        return;
    };
    let Some(rx) = active.audio_rx.take() else {
        return;
    };
    *epoch += 1;
    let _ = wire_cmd_tx.send(WireCmd::InstallAudio {
        rx,
        started_at: active.started_at,
        seek_base_secs: active.seek_base_secs,
        epoch: *epoch,
    });
}

/// PURA-396 §2a — the **control loop**. Spawns the [`run_wire_task`] and then
/// runs in place doing all the heavy work: chat dispatch, queue DB, pipeline
/// lifecycle, `BotEvent` broadcast. It no longer holds `&mut Connection`, so
/// it may stall freely — an ~11 s yt-dlp resolve here never gaps the wire,
/// because paced frames flow sibling → wire task directly and never traverse
/// this loop (the candidate-C fix).
///
/// Consumes `con` (moved into the wire task). Returns the same
/// [`ConnectedExit`] contract as `run_connected_loop`; the clean disconnect
/// happens inside the wire task on the caller-driven exits.
#[allow(clippy::too_many_arguments)]
async fn run_split_connected_loop(
    con: Connection,
    state: &mut BotState,
    current_channel: &mut Option<ChannelId>,
    rx: &mut mpsc::Receiver<BotCommand>,
    events: &broadcast::Sender<BotEvent>,
    bot_id: BotId,
    store: &Arc<dyn MusicBotStore>,
    yt_cookie: Arc<RwLock<Option<PathBuf>>>,
    yt_api_key: Arc<RwLock<Option<String>>>,
    bot_volume: VolumeHandle,
) -> ConnectedExit {
    let (wire_cmd_tx, wire_cmd_rx) = mpsc::unbounded_channel::<WireCmd>();
    let (wire_evt_tx, mut wire_evt_rx) = mpsc::unbounded_channel::<WireEvent>();
    let wire_handle = tokio::spawn(run_wire_task(con, wire_cmd_rx, wire_evt_tx, events.clone()));

    // Control-side audio state. `current_audio` is the pipeline lifecycle
    // handle (pause / seek / teardown); the frame-side counterpart lives in
    // the wire task's `WirePlay`. `audio_epoch` is the current pipeline
    // generation — see `install_pending_audio`.
    let mut current_audio: Option<ActiveAudio> = None;
    let mut audio_epoch: u64 = 0;

    let exit = loop {
        // Install any pipeline the previous iteration spawned.
        install_pending_audio(&mut current_audio, &mut audio_epoch, &wire_cmd_tx);

        tokio::select! {
            wevt = wire_evt_rx.recv() => match wevt {
                Some(WireEvent::Channel(channel)) => {
                    if Some(channel) != *current_channel {
                        *current_channel = Some(channel);
                        transition(state, BotState::InChannel, events);
                        let _ = events.send(BotEvent::JoinedChannel { channel_id: channel });
                    }
                }
                Some(WireEvent::Chat(lines)) => {
                    for msg in lines {
                        dispatch_chat_line(
                            &mut WireSink::Split(&wire_cmd_tx),
                            &mut current_audio,
                            bot_id,
                            store,
                            events,
                            &yt_cookie,
                            &yt_api_key,
                            &bot_volume,
                            &msg,
                        )
                        .await;
                    }
                }
                Some(WireEvent::Pipeline(ev)) => {
                    handle_audio_msg(
                        &mut WireSink::Split(&wire_cmd_tx),
                        Some(AudioMsg::PipelineEvent(ev)),
                        &mut current_audio,
                        bot_id,
                        store,
                        events,
                        &yt_cookie,
                        &bot_volume,
                    )
                    .await;
                }
                Some(WireEvent::AudioFinished { frames_sent, epoch }) => {
                    // Ignore a `Finished` from a pipeline we have since
                    // replaced (epoch mismatch).
                    if epoch == audio_epoch {
                        // The control-side `frames_sent` is never incremented
                        // in the split path (the wire task counts) — surface
                        // the wire count so `handle_audio_msg`'s 0-frame
                        // failure detection still works.
                        if let Some(active) = current_audio.as_mut() {
                            active.frames_sent = frames_sent;
                        }
                        handle_audio_msg(
                            &mut WireSink::Split(&wire_cmd_tx),
                            Some(AudioMsg::Finished),
                            &mut current_audio,
                            bot_id,
                            store,
                            events,
                            &yt_cookie,
                            &bot_volume,
                        )
                        .await;
                    }
                }
                Some(WireEvent::SiblingClosed { epoch }) => {
                    if epoch == audio_epoch {
                        handle_audio_msg(
                            &mut WireSink::Split(&wire_cmd_tx),
                            None,
                            &mut current_audio,
                            bot_id,
                            store,
                            events,
                            &yt_cookie,
                            &bot_volume,
                        )
                        .await;
                    }
                }
                Some(WireEvent::SendFailed { error, epoch }) => {
                    if epoch == audio_epoch {
                        error!(error, "send_audio failed — tearing down pipeline");
                        audio::tear_down(&mut current_audio);
                        // PURA-261 — `failed: ` prefix so `LivenessTracker`
                        // records `last_error` and drops the `Playing` state.
                        let _ = events.send(BotEvent::AudioFinished {
                            reason: format!("failed: audio send error — {error}"),
                        });
                    }
                }
                Some(WireEvent::Dropped(reason)) => break ConnectedExit::Dropped(reason),
                None => break ConnectedExit::Dropped("wire task ended".into()),
            },
            cmd = rx.recv() => match cmd {
                Some(BotCommand::Disconnect) => {
                    if audio::tear_down(&mut current_audio) {
                        let _ = wire_cmd_tx.send(WireCmd::ClearAudio);
                        let _ = wire_cmd_tx.send(WireCmd::VoiceStop);
                        let _ = events.send(BotEvent::AudioFinished {
                            reason: "disconnect".into(),
                        });
                    }
                    break ConnectedExit::Disconnect;
                }
                Some(BotCommand::Shutdown) => {
                    if audio::tear_down(&mut current_audio) {
                        let _ = wire_cmd_tx.send(WireCmd::ClearAudio);
                        let _ = wire_cmd_tx.send(WireCmd::VoiceStop);
                        let _ = events.send(BotEvent::AudioFinished {
                            reason: "shutdown".into(),
                        });
                    }
                    break ConnectedExit::Shutdown;
                }
                Some(BotCommand::Connect) => {
                    debug!("Connect ignored — already online");
                }
                Some(BotCommand::JoinChannel(target)) => {
                    // `channel_move` on a `Split` sink cannot fail (it just
                    // enqueues a `WireCmd`); the wire task logs any real
                    // send error and emits a `BotEvent::Error`.
                    let _ = WireSink::Split(&wire_cmd_tx).channel_move(target);
                }
                Some(BotCommand::LeaveChannel) => {
                    let _ = events.send(BotEvent::LeftChannel);
                    if let Some(id) = *current_channel {
                        debug!(channel_id = id, "LeaveChannel — staying in current channel until WS-3 default-channel tracking lands");
                    }
                }
                Some(BotCommand::Audio(audio_cmd)) => {
                    handle_audio_command(
                        &mut WireSink::Split(&wire_cmd_tx),
                        audio_cmd,
                        &mut current_audio,
                        bot_id,
                        store,
                        events,
                        &yt_cookie,
                        &bot_volume,
                    )
                    .await;
                }
                Some(BotCommand::Queue(qc)) => {
                    handle_queue_command(bot_id, store, qc, events).await;
                }
                None => break ConnectedExit::Dropped("command channel closed".into()),
            },
        }
    };

    // Caller-driven exits: tell the wire task to clean-disconnect, then wait
    // for it so the disconnect actually flushed before the actor's state
    // machine moves on. On a `Dropped` exit the wire task has already
    // returned; dropping `wire_cmd_tx` lets a blocked `recv` fall through.
    match &exit {
        ConnectedExit::Disconnect => {
            let _ = wire_cmd_tx.send(WireCmd::Disconnect { shutdown: false });
        }
        ConnectedExit::Shutdown => {
            let _ = wire_cmd_tx.send(WireCmd::Disconnect { shutdown: true });
        }
        ConnectedExit::Dropped(_) => {}
    }
    drop(wire_cmd_tx);
    let _ = wire_handle.await;
    exit
}

/// PURA-154 — drain a message from the audio sibling task.
///
/// PURA-358 — returns a `&'static str` naming the message kind handled, so
/// the connected loop's stall watchdog can attribute a slow audio-arm
/// iteration (e.g. a `finished` that ran an ~11 s yt-dlp auto-advance, or
/// `send_audio` contention on a `frame`).
#[allow(clippy::too_many_arguments)]
async fn handle_audio_msg(
    wire: &mut WireSink<'_>,
    msg: Option<AudioMsg>,
    current_audio: &mut Option<ActiveAudio>,
    bot_id: BotId,
    store: &Arc<dyn MusicBotStore>,
    events: &broadcast::Sender<BotEvent>,
    yt_cookie: &Arc<RwLock<Option<PathBuf>>>,
    bot_volume: &VolumeHandle,
) -> &'static str {
    let Some(msg) = msg else {
        // Sibling closed without sending Finished — treat as a hard stop.
        // This shouldn't happen in practice; the sibling always sends
        // Finished before its task body returns.
        warn!("audio sibling channel closed without Finished — tearing down");
        if audio::tear_down(current_audio) {
            wire.clear_audio();
            wire.voice_stop();
            let _ = events.send(BotEvent::AudioFinished {
                reason: "failed: audio pipeline channel closed unexpectedly".into(),
            });
        }
        return "sibling_closed";
    };
    match msg {
        AudioMsg::Frame { bytes, enqueued_at } => {
            // PURA-389a — the send + its A/B/C timing happen inside this
            // `if let` so `active` (and its `send_monitor`) is borrowed only
            // here; `send_result` owns its data, freeing `current_audio` for
            // the teardown branch below.
            let send_result = if let Some(active) = current_audio.as_mut() {
                active.frames_sent += 1;
                // PURA-330 — the end-to-end latency milestone: this is the
                // first Opus frame the operator can actually hear. One
                // INFO line per play closes out the `!play` → first-audio
                // breakdown started by the `music_bot_latency` stage logs.
                if active.frames_sent == 1 {
                    info!(
                        target: "music_bot_latency",
                        stage = "first_frame_on_wire",
                        elapsed_ms = active.started_at.elapsed().as_millis() as u64,
                        "first Opus frame sent on the wire — playback audible",
                    );
                    // THE-927 — clear any dashboard "Resolving
                    // YouTube…" pill the chat command lit up.
                    let _ = events.send(BotEvent::FirstFrameOnWire);
                }
                // PURA-347 — emit a once-per-second playback-progress
                // tick. `frames_sent` advances only on frames actually
                // delivered, so the elapsed clock stalls across a `Pause`
                // and never drifts. The FE reduces these into the
                // now-playing progress bar.
                //
                // PURA-352 — after a seek the pipeline restarts with
                // `frames_sent` back at 0, so the reported elapsed clock
                // is offset by `seek_base_secs` (the position the seek
                // jumped to).
                if active.frames_sent % FRAMES_PER_PROGRESS_TICK == 0 {
                    let _ = events.send(BotEvent::Progress {
                        elapsed_secs: active.seek_base_secs
                            + active.frames_sent / FRAMES_PER_PROGRESS_TICK,
                    });
                }
                wire.send_opus_frame(&bytes, enqueued_at, &mut active.send_monitor)
            } else {
                Ok(())
            };
            if let Err(err) = send_result {
                error!(?err, "send_audio failed — tearing down pipeline");
                audio::tear_down(current_audio);
                wire.clear_audio();
                // PURA-261 — `failed: ` prefix so `LivenessTracker`
                // surfaces this as the bot's `last_error` and the
                // synthesised `Playing` state drops.
                let _ = events.send(BotEvent::AudioFinished {
                    reason: format!("failed: audio send error — {err}"),
                });
            }
            "frame"
        }
        AudioMsg::PipelineEvent(PipelineEvent::NowPlaying { title, source }) => {
            // Synthesize an ephemeral `Track` so the wire surface (which
            // already accepts `BotEvent::NowPlaying(Track)` from the
            // queue path) carries the ICY metadata too. id=0 marks this
            // as "not a queue entry" — subscribers that care can match
            // on it. WS-7 may swap this for a richer event later.
            let track = Track {
                id: TrackId(0),
                source: AudioSource::Url(source),
                title,
                duration_secs: None,
                requested_by: None,
            };
            let _ = events.send(BotEvent::NowPlaying(track));
            "now_playing"
        }
        AudioMsg::PipelineEvent(PipelineEvent::Warning(message)) => {
            warn!(%message, "audio pipeline warning");
            // PURA-314 — stash the cause so a 0-frame `Finished` can build a
            // specific failure reason (e.g. the yt-dlp cookie gate) instead
            // of the generic "check yt-dlp/ffmpeg logs".
            if let Some(active) = current_audio.as_mut() {
                active.last_diagnostic = Some(message.clone());
            }
            let _ = events.send(BotEvent::Error(BotError::Internal(format!(
                "audio pipeline: {message}"
            ))));
            "pipeline_warning"
        }
        AudioMsg::PipelineEvent(PipelineEvent::EndOfStream) => {
            // Informational — the sibling will follow with `Finished`
            // once the frame channel drains.
            debug!("audio pipeline end-of-stream");
            "end_of_stream"
        }
        AudioMsg::Finished => {
            wire.voice_stop();
            // PURA-261 — a pipeline that drained without ever producing
            // a frame means yt-dlp / ffmpeg failed (bad URL, bot-gated
            // video, codec error). Flag it with the `failed: ` reason
            // prefix so `LivenessTracker` records `last_error` and the
            // synthesised `Playing` state drops — otherwise the bot
            // reports `Playing` forever with the cause log-only.
            let frames = current_audio.as_ref().map(|a| a.frames_sent).unwrap_or(0);
            let diagnostic = current_audio
                .as_ref()
                .and_then(|a| a.last_diagnostic.clone());
            let reason = if frames == 0 {
                // PURA-314 — prefer the captured pipeline diagnostic (yt-dlp
                // cookie gate, private/unavailable video, …) so the UI's
                // `last_error` banner tells the operator *why* playback
                // failed and what to do. Fall back to the generic message
                // only when nothing classified the failure.
                let cause = diagnostic.unwrap_or_else(|| {
                    "audio pipeline produced 0 frames — check yt-dlp/ffmpeg logs".to_string()
                });
                warn!(
                    %cause,
                    "audio pipeline finished with 0 frames — yt-dlp or ffmpeg failed"
                );
                format!("failed: {cause}")
            } else {
                "end_of_stream".to_string()
            };
            audio::tear_down(current_audio);
            // Emit `AudioFinished` BEFORE the queue advance so it clears
            // `now_playing` / `last_error` ahead of any `NowPlaying` the
            // auto-advance fires for the next track (which must win).
            let _ = events.send(BotEvent::AudioFinished { reason });
            handle_queue_command(bot_id, store, QueueCommand::Advance, events).await;
            auto_start_pending_track(current_audio, store, bot_id, events, yt_cookie, bot_volume)
                .await;
            "finished"
        }
    }
}

/// PURA-154 — dispatch one `AudioCommand`. Returns once the requested
/// state mutation has happened; the actual streaming continues on the
/// sibling task.
#[allow(clippy::too_many_arguments)]
async fn handle_audio_command(
    wire: &mut WireSink<'_>,
    cmd: AudioCommand,
    current_audio: &mut Option<ActiveAudio>,
    bot_id: BotId,
    store: &Arc<dyn MusicBotStore>,
    events: &broadcast::Sender<BotEvent>,
    yt_cookie: &Arc<RwLock<Option<PathBuf>>>,
    bot_volume: &VolumeHandle,
) {
    match cmd {
        AudioCommand::Play { source } => {
            // Direct Play bypasses the queue — the REST surface
            // (`/api/music-bots/{id}/play`) explicitly logs this case as
            // `track_id: None` (`audio_control.rs:65`). We tear down
            // any active pipeline and spawn a fresh one.
            //
            // PURA-190: emit at info-level so operators bisecting "did
            // Play even reach the supervisor?" can answer it with a
            // single grep against manager logs without enabling debug.
            // PURA-223 — read the current cookie path at play-time so a
            // UI-uploaded cookie takes effect without a manager restart.
            let cookie = yt_cookie.read().unwrap().clone();
            info!(?source, "AudioCommand::Play — spawning pipeline");
            if let Err(err) =
                audio::start_pipeline(current_audio, &source, cookie, bot_volume).await
            {
                warn!(?err, "audio pipeline spawn failed");
                let _ = events.send(BotEvent::Error(BotError::Internal(format!(
                    "audio pipeline spawn: {err}"
                ))));
                return;
            }
            // Emit a NowPlaying so subscribers light up — the pipeline's
            // own NowPlaying lands later for ICY/yt-dlp metadata.
            let label = match &source {
                AudioSource::Url(u) => u.clone(),
                AudioSource::LibraryPath(p) => p.to_string_lossy().into_owned(),
            };
            let track = Track {
                id: TrackId(0),
                source,
                title: label,
                duration_secs: None,
                requested_by: None,
            };
            let _ = events.send(BotEvent::NowPlaying(track));
        }
        AudioCommand::Stop => {
            if audio::tear_down(current_audio) {
                wire.clear_audio();
                wire.voice_stop();
                let _ = events.send(BotEvent::AudioFinished {
                    reason: "stopped".into(),
                });
            }
        }
        AudioCommand::Pause => {
            if let Some(active) = current_audio.as_ref() {
                active.set_paused(true);
            } else {
                debug!("Pause ignored — no active pipeline");
            }
        }
        AudioCommand::Resume => {
            if let Some(active) = current_audio.as_ref() {
                active.set_paused(false);
            } else {
                debug!("Resume ignored — no active pipeline");
            }
        }
        AudioCommand::SkipNext => {
            // Tear down the current pipeline, then advance the queue so
            // the post-advance head is the next track and auto-start it.
            let was_active = audio::tear_down(current_audio);
            if was_active {
                wire.clear_audio();
                wire.voice_stop();
            }
            // PURA-261 — emit `AudioFinished` BEFORE the queue advance:
            // `LivenessTracker` clears `now_playing` on `AudioFinished`,
            // so the next track's `NowPlaying` (fired by the advance)
            // must come after it to win.
            let _ = events.send(BotEvent::AudioFinished {
                reason: "skipped".into(),
            });
            handle_queue_command(bot_id, store, QueueCommand::Advance, events).await;
            auto_start_pending_track(current_audio, store, bot_id, events, yt_cookie, bot_volume)
                .await;
        }
        AudioCommand::Seek { secs } => {
            // PURA-352 — re-spawn the decoder for the current track at the
            // offset, reusing the resolved stream URL (no yt-dlp re-run).
            match audio::seek_to(current_audio, secs, bot_volume).await {
                Ok(true) => {
                    info!(secs, "AudioCommand::Seek — re-spawned pipeline at offset");
                    // Flush the wire so the TS jitter buffer drops the gap
                    // between the old and the post-seek frames cleanly.
                    wire.voice_stop();
                    // Snap the FE progress clock to the seek target now —
                    // the next `Progress` tick (offset + frames/50) only
                    // lands a second into the post-seek pre-buffer.
                    let _ = events.send(BotEvent::Progress { elapsed_secs: secs });
                }
                Ok(false) => {
                    debug!(
                        secs,
                        "Seek ignored — no active pipeline or track not yet seekable"
                    );
                }
                Err(err) => {
                    warn!(?err, "seek pipeline respawn failed");
                    let _ =
                        events.send(BotEvent::Error(BotError::Internal(format!("seek: {err}"))));
                }
            }
        }
        AudioCommand::SetVolume(gain) => {
            // PURA-351 — apply the operator's output gain. `bot_volume` is
            // the shared handle every pipeline this bot spawns holds a
            // clone of, so the change lands on the live track immediately
            // and is inherited by every later track and reconnect. No
            // pipeline need be active — the value is staged for next play.
            bot_volume.set(gain);
            debug!(gain = bot_volume.get(), "AudioCommand::SetVolume applied");
        }
        // SkipPrev / NowPlaying don't have pipeline support yet — leave
        // them on the stub path so REST/UI subscribers see a
        // dispatched-but-unsupported event.
        other => emit_audio_stub(events, &other),
    }
}

/// PURA-154 — if the queue has a head and no pipeline is currently
/// spawned, start one for the head track. Returns `true` when a
/// pipeline was started.
async fn auto_start_pending_track(
    current_audio: &mut Option<ActiveAudio>,
    store: &Arc<dyn MusicBotStore>,
    bot_id: BotId,
    events: &broadcast::Sender<BotEvent>,
    yt_cookie: &Arc<RwLock<Option<PathBuf>>>,
    bot_volume: &VolumeHandle,
) -> bool {
    if current_audio.is_some() {
        return false;
    }
    let next = match store.queue_current(bot_id).await {
        Ok(Some(track)) => track,
        Ok(None) => return false,
        Err(err) => {
            emit_store_error(events, "queue_current", err);
            return false;
        }
    };
    let cookie = yt_cookie.read().unwrap().clone();
    if let Err(err) = audio::start_pipeline(current_audio, &next.source, cookie, bot_volume).await {
        warn!(?err, "auto-start audio pipeline failed");
        // PURA-340 — emit `AudioFinished` with the `failed: ` prefix, not
        // a bare `Error`. Every caller of this function (chat `!play`,
        // `!radio`, `!skip`, and the queue auto-advance after a track
        // ends) has already emitted a `NowPlaying` for the head track, so
        // a bare `Error` — which `LivenessTracker` ignores — would leave
        // the bot reporting `playing` forever. `AudioFinished` clears
        // `now_playing` and records the cause as `last_error`, the same
        // contract a 0-frame pipeline finish uses.
        let _ = events.send(BotEvent::AudioFinished {
            reason: format!("failed: audio pipeline spawn — {err}"),
        });
        return false;
    }
    true
}

/// Translate a `StreamItem` into "we are now in channel X if you care".
/// Anything else is logged at debug.
fn handle_stream_item(item: StreamItem, con: &Connection) -> Option<ChannelId> {
    match item {
        StreamItem::BookEvents(_) => {
            // The book has been updated; pull the own client's current
            // channel off it so we always reflect the authoritative
            // server state.
            let book = con.get_state().ok()?;
            let own = book.clients.get(&book.own_client)?;
            Some(own.channel.0)
        }
        StreamItem::DisconnectedTemporarily(reason) => {
            warn!(?reason, "temporary disconnect — tsclientlib will retry");
            None
        }
        StreamItem::IdentityLevelIncreasing(level) => {
            info!(level, "server requires higher identity — upgrading");
            None
        }
        StreamItem::IdentityLevelIncreased => {
            info!("identity upgraded — handshake will resume");
            None
        }
        other => {
            debug!(?other, "stream item");
            None
        }
    }
}

/// Build + send the channel-move command. The bookkeeping crate generates
/// `Client::set_channel(ChannelId) -> OutClientMovePart`; the
/// `OutCommandExt::send` trait turns the `Out…Part` into bytes on the wire.
fn send_channel_move(con: &mut Connection, target: ChannelId) -> Result<()> {
    let book = con.get_state().context("connection has no book yet")?;
    let own = book
        .clients
        .get(&book.own_client)
        .context("own client not present in book")?;
    let cmd = own.client_move(TsChannelId(target));
    cmd.send(con).context("OutCommandExt::send (client-move)")?;
    Ok(())
}

/// Run the handshake. On success returns the live connection plus the
/// initial channel ID and the bot's own client ID for the
/// `BotEvent::Connected` payload.
async fn attempt_connect(config: &BotConfig) -> Result<(Connection, ClientId, ChannelId)> {
    let identity = load_or_create_identity(&config.identity_path)
        .await
        .context("load_or_create_identity")?;

    let mut con = Connection::build(config.server_addr.as_str())
        .name(config.name.clone())
        .identity(identity)
        .log_commands(false)
        .log_packets(false)
        .log_udp_packets(false)
        .connect()
        .context("Connection::build()")?;

    let connected = wait_for_connected(&mut con, config.handshake_timeout)
        .await
        .context("handshake driver")?;
    if !connected {
        anyhow::bail!(
            "handshake did not complete within {:?} — fixture up?",
            config.handshake_timeout
        );
    }

    // The first `BookEvents` resolves `wait_for_connected`, but the
    // book's `clients[own_client]` entry may still be in flight — TS6
    // sends the `clientlist` notification a few packets after
    // `InitServer`. Drive the event stream briefly until the entry
    // shows up so callers always get an authoritative `default_channel`
    // (`channeltree.rs` upstream sleeps 1 s for the same reason).
    let (own_id, default_channel) = wait_for_own_client(&mut con, Duration::from_secs(2)).await?;

    Ok((con, own_id, default_channel))
}

/// Drain the event stream until the bot's own `Client` entry appears in
/// the book (or `timeout` fires). Returns the own `ClientId` plus the
/// channel the server placed us in.
async fn wait_for_own_client(
    con: &mut Connection,
    timeout: Duration,
) -> Result<(ClientId, ChannelId)> {
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        if let Some(pair) = read_own_client(con) {
            return Ok(pair);
        }
        // Same borrow-checker dance as `run_connected_loop`: the events
        // stream holds `&mut con`, so build it inline as the select arm
        // and let it drop before the next iteration peeks at the book.
        tokio::select! {
            biased;
            _ = &mut deadline => {
                anyhow::bail!(
                    "post-handshake: own client never appeared in book within {timeout:?}"
                );
            }
            ev = async { con.events().next().await } => match ev {
                Some(Ok(_)) => continue,
                Some(Err(err)) => {
                    return Err(anyhow::anyhow!(
                        "stream error while waiting for own client: {err}"
                    ));
                }
                None => anyhow::bail!("stream ended before own client appeared in book"),
            }
        }
    }
}

/// Read the own client + its current channel out of the connection
/// book. Returns `None` if the book exists but the entry is not present
/// yet (the typical post-handshake transient).
fn read_own_client(con: &Connection) -> Option<(ClientId, ChannelId)> {
    let book = con.get_state().ok()?;
    let own = book.clients.get(&book.own_client)?;
    Some((book.own_client, own.channel.0))
}

/// Send a clean `clientdisconnect` message and let the stream drain so
/// the server processes it. Mirrors `ts6-voice-prototype`'s tear-down.
async fn clean_disconnect(con: &mut Connection, reason: &str) {
    if let Err(err) = con.disconnect(
        DisconnectOptions::new()
            .reason(Reason::Clientdisconnect)
            .message(reason.to_string()),
    ) {
        warn!(?err, "disconnect command failed (non-fatal)");
    }
    let drain_deadline = Instant::now() + Duration::from_secs(2);
    let drain = con.events();
    tokio::pin!(drain);
    while Instant::now() < drain_deadline {
        match tokio::time::timeout(Duration::from_millis(250), drain.next()).await {
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(_) => continue,
        }
    }
}

fn transition(state: &mut BotState, to: BotState, events: &broadcast::Sender<BotEvent>) {
    match state.transition(to) {
        Ok(new) => {
            let from = *state;
            *state = new;
            let _ = events.send(BotEvent::StateChanged { from, to });
        }
        Err(err) => {
            error!(?err, "illegal state transition — staying put");
            let _ = events.send(BotEvent::Error(BotError::Internal(format!(
                "illegal transition: {err}"
            ))));
        }
    }
}

fn emit_rejected(events: &broadcast::Sender<BotEvent>, cmd: &BotCommand, state: BotState) {
    let label = command_label(cmd);
    warn!(
        command = label,
        ?state,
        "command rejected for current state"
    );
    let _ = events.send(BotEvent::Error(BotError::CommandRejected {
        command: label.into(),
        state,
    }));
}

/// PURA-121 WS-3 — translate a `QueueCommand` into store mutations and
/// post-mutation `BotEvent`s. Shared by the `Disconnected` branch (for
/// pre-connect queue staging) and the connected loop (for in-session
/// mutations + auto-advance).
async fn handle_queue_command(
    bot_id: BotId,
    store: &Arc<dyn MusicBotStore>,
    cmd: QueueCommand,
    events: &broadcast::Sender<BotEvent>,
) {
    match cmd {
        QueueCommand::Enqueue(track) => {
            let was_empty = store
                .queue_peek(bot_id)
                .await
                .map(|q| q.is_empty())
                .unwrap_or(false);
            match store.queue_enqueue(bot_id, track).await {
                Ok(track) => {
                    emit_queue_changed(store, bot_id, events).await;
                    if was_empty {
                        let _ = events.send(BotEvent::NowPlaying(track));
                    }
                }
                Err(err) => emit_store_error(events, "queue_enqueue", err),
            }
        }
        QueueCommand::EnqueuePlaylist(name) => {
            let was_empty = store
                .queue_peek(bot_id)
                .await
                .map(|q| q.is_empty())
                .unwrap_or(false);
            match store.enqueue_playlist(bot_id, &name).await {
                Ok(stamped) => {
                    emit_queue_changed(store, bot_id, events).await;
                    if let Some(first) = stamped.into_iter().next().filter(|_| was_empty) {
                        let _ = events.send(BotEvent::NowPlaying(first));
                    }
                }
                Err(err) => emit_store_error(events, "enqueue_playlist", err),
            }
        }
        QueueCommand::Remove(id) => {
            let head_before = store.queue_current(bot_id).await.ok().flatten();
            let removed_head = head_before.as_ref().map(|t| t.id) == Some(id);
            match store.queue_remove(bot_id, id).await {
                Ok(true) => {
                    emit_queue_changed(store, bot_id, events).await;
                    if removed_head {
                        emit_head_change(store, bot_id, events).await;
                    }
                }
                Ok(false) => {
                    debug!(?id, "queue_remove no-op — id not in queue");
                }
                Err(err) => emit_store_error(events, "queue_remove", err),
            }
        }
        QueueCommand::Reorder(order) => {
            let head_before = store
                .queue_current(bot_id)
                .await
                .ok()
                .flatten()
                .map(|t| t.id);
            match store.queue_reorder(bot_id, order).await {
                Ok(()) => {
                    emit_queue_changed(store, bot_id, events).await;
                    let head_after = store.queue_current(bot_id).await.ok().flatten();
                    if head_after.as_ref().map(|t| t.id) != head_before {
                        if let Some(track) = head_after {
                            let _ = events.send(BotEvent::NowPlaying(track));
                        } else {
                            let _ = events.send(BotEvent::QueueEmpty);
                        }
                    }
                }
                Err(err) => emit_store_error(events, "queue_reorder", err),
            }
        }
        QueueCommand::Clear => {
            let was_non_empty = store
                .queue_peek(bot_id)
                .await
                .map(|q| !q.is_empty())
                .unwrap_or(false);
            match store.queue_clear(bot_id).await {
                Ok(()) => {
                    emit_queue_changed(store, bot_id, events).await;
                    if was_non_empty {
                        let _ = events.send(BotEvent::QueueEmpty);
                    }
                }
                Err(err) => emit_store_error(events, "queue_clear", err),
            }
        }
        QueueCommand::Advance => match store.queue_dequeue_head(bot_id).await {
            Ok(_popped) => {
                emit_queue_changed(store, bot_id, events).await;
                emit_head_change(store, bot_id, events).await;
            }
            Err(err) => emit_store_error(events, "queue_dequeue_head", err),
        },
    }
}

async fn emit_queue_changed(
    store: &Arc<dyn MusicBotStore>,
    bot_id: BotId,
    events: &broadcast::Sender<BotEvent>,
) {
    let queue: Vec<Track> = store.queue_peek(bot_id).await.unwrap_or_default();
    let current = queue.first().cloned();
    let _ = events.send(BotEvent::QueueChanged {
        len: queue.len(),
        current,
    });
}

/// Emit `NowPlaying(new_head)` if the queue still has a head, else
/// `QueueEmpty`. Called after every op that may have changed the head.
async fn emit_head_change(
    store: &Arc<dyn MusicBotStore>,
    bot_id: BotId,
    events: &broadcast::Sender<BotEvent>,
) {
    match store.queue_current(bot_id).await {
        Ok(Some(track)) => {
            let _ = events.send(BotEvent::NowPlaying(track));
        }
        Ok(None) => {
            let _ = events.send(BotEvent::QueueEmpty);
        }
        Err(err) => emit_store_error(events, "queue_current", err),
    }
}

fn emit_store_error(events: &broadcast::Sender<BotEvent>, op: &str, err: StoreError) {
    warn!(op, ?err, "store op failed");
    let _ = events.send(BotEvent::Error(BotError::Store {
        op: op.into(),
        message: err.to_string(),
    }));
}

/// PURA-154 — stub for audio sub-commands the pipeline doesn't cover
/// today. Only `SkipPrev` / `NowPlaying` route through here; Play / Stop /
/// Pause / Resume / SkipNext / SetVolume (PURA-351) flow into the real
/// pipeline dispatch in `handle_audio_command`.
fn emit_audio_stub(events: &broadcast::Sender<BotEvent>, cmd: &AudioCommand) {
    let label = match cmd {
        AudioCommand::SkipPrev => "SkipPrev".into(),
        AudioCommand::NowPlaying(s) => format!("NowPlaying({s})"),
        // The wired-up commands should never land here; if they do,
        // it's a routing bug — log loudly.
        other => {
            error!(
                ?other,
                "emit_audio_stub reached for a wired audio command — routing bug"
            );
            format!("{other:?}")
        }
    };
    debug!(command = %label, "audio command not yet supported");
    let _ = events.send(BotEvent::Error(BotError::AudioNotImplemented(label)));
}

/// Public re-export so callers can build a `Bot` actor directly without
/// going through the supervisor (used in unit tests).
#[allow(dead_code)]
pub(crate) fn arc_for_tests<T>(t: T) -> Arc<T> {
    Arc::new(t)
}

/// PURA-122 WS-4 — one chat line we'll feed through the parser, paired
/// with the invoker's name for debug-logging context.
struct ChatLine {
    invoker: String,
    text: String,
}

/// Pluck out incoming `MessageTarget::Channel` chat lines from a
/// `BookEvents` `StreamItem`. We deliberately filter:
/// - **Target**: only `Channel` (server-wide and private chat go elsewhere
///   — and they aren't what `!`-commands operate on).
/// - **Invoker**: skip the bot's own client id, otherwise the bot would
///   parse its own replies if a reply ever started with `!`.
///
/// Returns an empty `Vec` for non-`BookEvents` items and for items that
/// carried no channel chat — the common case stays cheap.
fn extract_channel_chat(item: &StreamItem, con: &Connection) -> Vec<ChatLine> {
    let StreamItem::BookEvents(events) = item else {
        return Vec::new();
    };
    let own_client_id = match con.get_state() {
        Ok(book) => Some(book.own_client),
        Err(_) => None,
    };
    let mut out = Vec::new();
    for ev in events {
        if let BookEvent::Message {
            target: MessageTarget::Channel,
            invoker,
            message,
        } = ev
        {
            if Some(invoker.id) == own_client_id {
                continue;
            }
            out.push(ChatLine {
                invoker: invoker.name.clone(),
                text: message.clone(),
            });
        }
    }
    out
}

/// Parse a chat line, then either dispatch it (real command) or send a
/// short error reply (parse error with a user-visible cause). Empty /
/// unknown lines are silently dropped per the issue spec.
///
/// PURA-340 — a queue-mutating command (`!play` / `!radio` / `!skip` /
/// `!stop`) returns a [`chat::ChatAudioAction`]; this function applies it
/// to the live pipeline. Before the fix the chat bridge had no path to
/// `current_audio` at all, so chat `!play` could only ever enqueue.
#[allow(clippy::too_many_arguments)]
async fn dispatch_chat_line(
    wire: &mut WireSink<'_>,
    current_audio: &mut Option<ActiveAudio>,
    bot_id: BotId,
    store: &Arc<dyn MusicBotStore>,
    events: &broadcast::Sender<BotEvent>,
    yt_cookie: &Arc<RwLock<Option<PathBuf>>>,
    yt_api_key: &Arc<RwLock<Option<String>>>,
    bot_volume: &VolumeHandle,
    msg: &ChatLine,
) {
    match chat::parse(&msg.text) {
        Ok(parsed) => {
            // PURA-330 — INFO so the chat-command-received timestamp is in
            // the manager log without enabling debug. This is stage 0 of
            // the `!play` → first-audio latency breakdown: the gap between
            // this line and `AudioCommand::Play — spawning pipeline` is
            // the command-dispatch latency the issue calls out as
            // previously unmeasured.
            info!(target: "music_bot_latency", invoker = %msg.invoker, command = ?parsed, "chat command received");
            // PURA-396 — `chat::handle_command` is `Connection`-free; the
            // reply rides the `WireSink` (a direct `send_reply`, or a
            // `WireCmd::ChatReply` to the wire task in the split path).
            // THE-948 — read the live API key from the `RwLock` at dispatch
            // time so a key saved in `/settings` takes effect without a
            // restart; `None`/empty falls back to the `ytsearch1:` path.
            let api_key = yt_api_key.read().unwrap().clone();
            let (reply, action) =
                chat::handle_command(bot_id, store, events, parsed, api_key.as_deref()).await;
            wire.chat_reply(reply);
            apply_chat_audio_action(
                wire,
                current_audio,
                bot_id,
                store,
                events,
                yt_cookie,
                bot_volume,
                action,
            )
            .await;
        }
        Err(err) => {
            debug!(invoker = %msg.invoker, line = %msg.text, ?err, "chat parse outcome");
            if let Some(reply) = chat::parse_error_reply(&err) {
                wire.chat_reply(reply);
            }
        }
    }
}

/// PURA-340 — execute the [`chat::ChatAudioAction`] a chat command
/// produced. The chat bridge mutates the queue and decides *what* should
/// happen to playback; only the connected loop owns `current_audio`, so
/// the actual pipeline spawn / teardown happens here.
#[allow(clippy::too_many_arguments)]
async fn apply_chat_audio_action(
    wire: &mut WireSink<'_>,
    current_audio: &mut Option<ActiveAudio>,
    bot_id: BotId,
    store: &Arc<dyn MusicBotStore>,
    events: &broadcast::Sender<BotEvent>,
    yt_cookie: &Arc<RwLock<Option<PathBuf>>>,
    bot_volume: &VolumeHandle,
    action: chat::ChatAudioAction,
) {
    use chat::ChatAudioAction;
    match action {
        ChatAudioAction::None => {}
        ChatAudioAction::Pause => {
            // PURA-353 — `!pause` parks the live pipeline's sibling, the
            // same `watch` flip the REST `pause_bot` control uses via
            // `AudioCommand::Pause`. A no-op when nothing is playing.
            if let Some(active) = current_audio.as_ref() {
                active.set_paused(true);
            } else {
                debug!("chat !pause ignored — no active pipeline");
            }
        }
        ChatAudioAction::Resume => {
            if let Some(active) = current_audio.as_ref() {
                active.set_paused(false);
            } else {
                debug!("chat !resume ignored — no active pipeline");
            }
        }
        ChatAudioAction::SetVolume(gain) => {
            // PURA-351 — `!vol` lowers here. Same shared handle the REST
            // `SetVolume` command and every pipeline use; applies live.
            bot_volume.set(gain);
            debug!(gain = bot_volume.get(), "chat !vol applied");
        }
        ChatAudioAction::StartIfIdle => {
            // `!play` — start the queue head. A no-op when a pipeline is
            // already live: the enqueued track stays queued and plays
            // when the current one finishes (`AudioMsg::Finished` →
            // `auto_start_pending_track`).
            auto_start_pending_track(current_audio, store, bot_id, events, yt_cookie, bot_volume)
                .await;
        }
        ChatAudioAction::RestartHead => {
            // `!radio` / `!skip` replaced the queue head. Tear the old
            // pipeline down WITHOUT emitting `AudioFinished`: the chat
            // handler already emitted `NowPlaying` for the new head, and
            // `AudioFinished` would clear that fresh `now_playing`.
            if audio::tear_down(current_audio) {
                wire.clear_audio();
                wire.voice_stop();
                debug!("chat command replaced the queue head — restarting pipeline");
            }
            auto_start_pending_track(current_audio, store, bot_id, events, yt_cookie, bot_volume)
                .await;
        }
        ChatAudioAction::StopPlayback => {
            // `!stop` / `!skip` past the last track. The chat handler
            // already drained the queue; tear the pipeline down and emit
            // `AudioFinished` so `LivenessTracker` drops the `Playing`
            // state. Mirrors `AudioCommand::Stop`.
            if audio::tear_down(current_audio) {
                wire.clear_audio();
                wire.voice_stop();
                let _ = events.send(BotEvent::AudioFinished {
                    reason: "stopped".into(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{InMemoryMusicBotStore, NewTrack};

    /// PURA-340 regression — the second half of the chat `!play` fix.
    /// `chat::dispatch` returns `StartIfIdle`; the connected loop applies
    /// it by calling `auto_start_pending_track`, which must turn a queued
    /// head into a live pipeline when the bot is idle. The bug was that
    /// nothing ever called this after a chat enqueue, so an idle bot's
    /// `!play` produced no audio.
    ///
    /// Uses the `synthetic://` source seam so a real pipeline spawns with
    /// no network / yt-dlp (see `audio::source_to_spec`).
    #[tokio::test]
    async fn auto_start_spawns_pipeline_for_queued_head() {
        let store: Arc<dyn MusicBotStore> = Arc::new(InMemoryMusicBotStore::new());
        let bot_id = BotId(1);
        store
            .queue_enqueue(
                bot_id,
                NewTrack {
                    source: AudioSource::Url(
                        "synthetic://?hz=440&duration_ms=200&amplitude=0.5".into(),
                    ),
                    title: "test tone".into(),
                    duration_secs: None,
                    requested_by: None,
                },
            )
            .await
            .expect("enqueue synthetic head");

        let (events, _rx) = broadcast::channel(64);
        let yt_cookie: Arc<RwLock<Option<PathBuf>>> = Arc::new(RwLock::new(None));
        let mut current_audio: Option<ActiveAudio> = None;

        let started = auto_start_pending_track(
            &mut current_audio,
            &store,
            bot_id,
            &events,
            &yt_cookie,
            &VolumeHandle::default(),
        )
        .await;
        assert!(
            started,
            "an idle bot with a queued head must spawn a pipeline"
        );
        assert!(
            current_audio.is_some(),
            "current_audio must hold the live pipeline after a spawn",
        );

        // Idempotency: a second call must not spawn a competing pipeline
        // while one is already running — this is what makes `StartIfIdle`
        // safe to return from `!play` unconditionally.
        let again = auto_start_pending_track(
            &mut current_audio,
            &store,
            bot_id,
            &events,
            &yt_cookie,
            &VolumeHandle::default(),
        )
        .await;
        assert!(!again, "no second pipeline while one is already live");
    }

    /// An idle bot with an empty queue has nothing to start — `!play`
    /// that failed to resolve must not leave a phantom pipeline.
    #[tokio::test]
    async fn auto_start_is_a_noop_on_empty_queue() {
        let store: Arc<dyn MusicBotStore> = Arc::new(InMemoryMusicBotStore::new());
        let (events, _rx) = broadcast::channel(64);
        let yt_cookie: Arc<RwLock<Option<PathBuf>>> = Arc::new(RwLock::new(None));
        let mut current_audio: Option<ActiveAudio> = None;

        let started = auto_start_pending_track(
            &mut current_audio,
            &store,
            BotId(1),
            &events,
            &yt_cookie,
            &VolumeHandle::default(),
        )
        .await;
        assert!(!started);
        assert!(current_audio.is_none());
    }

    /// PURA-358 — `command_label` is the stable name the connected-loop
    /// stall watchdog and rejected-command logging report. Every
    /// `BotCommand` variant must map to its expected label so a logged
    /// `connected_loop_stall arm=command` is unambiguous.
    #[test]
    fn command_label_covers_every_variant() {
        let cases: [(BotCommand, &str); 7] = [
            (BotCommand::Connect, "Connect"),
            (BotCommand::Disconnect, "Disconnect"),
            (BotCommand::JoinChannel(0), "JoinChannel"),
            (BotCommand::LeaveChannel, "LeaveChannel"),
            (BotCommand::Shutdown, "Shutdown"),
            (BotCommand::Audio(AudioCommand::Stop), "Audio"),
            (BotCommand::Queue(QueueCommand::Clear), "Queue"),
        ];
        for (cmd, expect) in cases {
            assert_eq!(command_label(&cmd), expect);
        }
    }

    /// PURA-358 — the stall watchdog must fire *below* the 20 ms audio-frame
    /// cadence. A threshold at or above the cadence would only log a stall
    /// after it had already gapped the wire, defeating the early-warning
    /// intent that lets an operator attribute a `frame_underrun`.
    #[test]
    fn loop_stall_warn_is_below_the_frame_cadence() {
        assert!(
            LOOP_STALL_WARN < Duration::from_millis(20),
            "a stall watchdog at/above the 20 ms frame cadence cannot warn early",
        );
        // `log_loop_stall`'s detail closure must not run on a fast iteration.
        let cell = std::cell::Cell::new(false);
        log_loop_stall("event", Instant::now(), || {
            cell.set(true);
            String::new()
        });
        assert!(
            !cell.get(),
            "a sub-threshold iteration must not even format the stall detail",
        );
    }

    /// PURA-396 — the `VOICE_SPLIT_WIRE_TASK` env override is parsed
    /// trimmed and case-insensitive on the common truthy spellings;
    /// anything else (incl. absent) is off.
    #[test]
    fn split_flag_truthy_accepts_the_common_spellings() {
        for v in ["1", "true", "TRUE", " yes ", "On", "tRuE"] {
            assert!(split_flag_truthy(Some(v)), "{v:?} should be truthy");
        }
        for v in ["0", "false", "", "no", "off", "2"] {
            assert!(!split_flag_truthy(Some(v)), "{v:?} should be falsy");
        }
        assert!(!split_flag_truthy(None), "an unset var is off");
    }

    /// PURA-396 — the acceptance criterion: a single FIFO `mpsc<WireCmd>`
    /// orders audio teardown vs. voice-stop. A `WireSink::Split` issues
    /// each command in call order, so `clear_audio()` (drop buffered
    /// frames) lands before `voice_stop()` on the wire.
    #[test]
    fn wire_sink_split_preserves_command_fifo() {
        let (tx, mut rx) = mpsc::unbounded_channel::<WireCmd>();
        {
            let mut wire = WireSink::Split(&tx);
            wire.clear_audio();
            wire.voice_stop();
            wire.chat_reply("hello".to_string());
            assert!(
                wire.channel_move(7).is_ok(),
                "a split channel_move never fails"
            );
        }
        assert!(matches!(rx.try_recv(), Ok(WireCmd::ClearAudio)));
        assert!(matches!(rx.try_recv(), Ok(WireCmd::VoiceStop)));
        assert!(matches!(rx.try_recv(), Ok(WireCmd::ChatReply(line)) if line == "hello"),);
        assert!(matches!(rx.try_recv(), Ok(WireCmd::ChannelMove(7))));
        assert!(rx.try_recv().is_err(), "exactly four commands issued");
    }

    /// PURA-396 — `install_pending_audio` hands a freshly-spawned
    /// pipeline's frame receiver to the wire task exactly once, bumping
    /// the pipeline-generation `epoch`; a second call with the receiver
    /// already taken is an epoch-stable no-op. The `epoch` is what lets
    /// the control task discard a teardown event from a replaced
    /// pipeline. Uses the `synthetic://` seam — no network / yt-dlp.
    #[tokio::test]
    async fn install_pending_audio_ships_receiver_once_and_bumps_epoch() {
        let store: Arc<dyn MusicBotStore> = Arc::new(InMemoryMusicBotStore::new());
        let bot_id = BotId(1);
        store
            .queue_enqueue(
                bot_id,
                NewTrack {
                    source: AudioSource::Url(
                        "synthetic://?hz=440&duration_ms=200&amplitude=0.5".into(),
                    ),
                    title: "tone".into(),
                    duration_secs: None,
                    requested_by: None,
                },
            )
            .await
            .expect("enqueue synthetic head");
        let (events, _rx) = broadcast::channel(64);
        let yt_cookie: Arc<RwLock<Option<PathBuf>>> = Arc::new(RwLock::new(None));
        let mut current_audio: Option<ActiveAudio> = None;
        auto_start_pending_track(
            &mut current_audio,
            &store,
            bot_id,
            &events,
            &yt_cookie,
            &VolumeHandle::default(),
        )
        .await;
        assert!(
            current_audio.as_ref().is_some_and(|a| a.audio_rx.is_some()),
            "a freshly-spawned pipeline carries an un-taken frame receiver",
        );

        let (tx, mut rx) = mpsc::unbounded_channel::<WireCmd>();
        let mut epoch = 0u64;
        install_pending_audio(&mut current_audio, &mut epoch, &tx);
        assert_eq!(epoch, 1, "the first install bumps the generation epoch");
        assert!(
            matches!(rx.try_recv(), Ok(WireCmd::InstallAudio { epoch: 1, .. })),
            "the receiver is shipped to the wire task tagged with the epoch",
        );
        assert!(
            current_audio.as_ref().unwrap().audio_rx.is_none(),
            "the receiver was taken — the control task no longer drains frames",
        );

        // No fresh pipeline ⇒ the receiver is already gone ⇒ no-op.
        install_pending_audio(&mut current_audio, &mut epoch, &tx);
        assert_eq!(
            epoch, 1,
            "a second call with nothing pending is epoch-stable"
        );
        assert!(rx.try_recv().is_err(), "and ships no further WireCmd");
    }
}
