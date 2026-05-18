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
                    Ok((mut con, client_id, default_channel)) => {
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
                        let outcome = run_connected_loop(
                            &mut con,
                            &mut state,
                            &mut current_channel,
                            &mut rx,
                            &events,
                            bot_id,
                            &store,
                            Arc::clone(&yt_cookie),
                            bot_volume.clone(),
                        )
                        .await;
                        match outcome {
                            ConnectedExit::Shutdown => {
                                shutdown_requested = true;
                                // Connected/InChannel → Disconnecting → Disconnected.
                                // The state machine rejects skipping
                                // Disconnecting; honour both transitions
                                // so the public event log is correct.
                                transition(&mut state, BotState::Disconnecting, &events);
                                clean_disconnect(&mut con, "shutdown").await;
                                transition(&mut state, BotState::Disconnected, &events);
                                let _ = events.send(BotEvent::Disconnected {
                                    kind: DisconnectKind::ShutdownRequested,
                                    reason: "shutdown".into(),
                                });
                                connection = None;
                            }
                            ConnectedExit::Disconnect => {
                                transition(&mut state, BotState::Disconnecting, &events);
                                clean_disconnect(&mut con, "disconnect").await;
                                transition(&mut state, BotState::Disconnected, &events);
                                let _ = events.send(BotEvent::Disconnected {
                                    kind: DisconnectKind::Clean,
                                    reason: "disconnect".into(),
                                });
                                connection = None;
                            }
                            ConnectedExit::Dropped(reason) => {
                                warn!(%reason, "connection dropped — auto-reconnect");
                                drop(con);
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
                // Unwrap is sound because the guard below gates entry.
                current_audio.as_mut().unwrap().audio_rx.recv().await
            }, if current_audio.is_some() => {
                // PURA-358 — time the audio-arm body. `send_audio`
                // contention on a `frame`, or an ~11 s yt-dlp auto-advance
                // on a `finished`, both stall the loop here.
                let arm_start = Instant::now();
                let kind = handle_audio_msg(
                    con,
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
                            con,
                            &mut current_audio,
                            bot_id,
                            store,
                            events,
                            &yt_cookie,
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
                        con,
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

/// PURA-154 — drain a message from the audio sibling task.
///
/// PURA-358 — returns a `&'static str` naming the message kind handled, so
/// the connected loop's stall watchdog can attribute a slow audio-arm
/// iteration (e.g. a `finished` that ran an ~11 s yt-dlp auto-advance, or
/// `send_audio` contention on a `frame`).
#[allow(clippy::too_many_arguments)]
async fn handle_audio_msg(
    con: &mut Connection,
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
            audio::send_voice_stop(con);
            let _ = events.send(BotEvent::AudioFinished {
                reason: "failed: audio pipeline channel closed unexpectedly".into(),
            });
        }
        return "sibling_closed";
    };
    match msg {
        AudioMsg::Frame(opus) => {
            if let Some(active) = current_audio.as_mut() {
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
            }
            if let Err(err) = audio::send_opus_frame(con, &opus) {
                error!(?err, "send_audio failed — tearing down pipeline");
                audio::tear_down(current_audio);
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
            audio::send_voice_stop(con);
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
    con: &mut Connection,
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
                audio::send_voice_stop(con);
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
                audio::send_voice_stop(con);
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
                    audio::send_voice_stop(con);
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
    con: &mut Connection,
    current_audio: &mut Option<ActiveAudio>,
    bot_id: BotId,
    store: &Arc<dyn MusicBotStore>,
    events: &broadcast::Sender<BotEvent>,
    yt_cookie: &Arc<RwLock<Option<PathBuf>>>,
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
            let action = chat::dispatch(bot_id, con, store, events, parsed).await;
            apply_chat_audio_action(
                con,
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
            chat::reply_for_parse_error(con, &err);
        }
    }
}

/// PURA-340 — execute the [`chat::ChatAudioAction`] a chat command
/// produced. The chat bridge mutates the queue and decides *what* should
/// happen to playback; only the connected loop owns `current_audio`, so
/// the actual pipeline spawn / teardown happens here.
#[allow(clippy::too_many_arguments)]
async fn apply_chat_audio_action(
    con: &mut Connection,
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
                audio::send_voice_stop(con);
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
                audio::send_voice_stop(con);
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
}
