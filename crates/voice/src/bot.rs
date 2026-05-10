//! Bot actor — PURA-118 WS-1.
//!
//! One actor task per bot. Owns the `Connection`, drives the lifecycle
//! state machine, dispatches `BotCommand`s, and emits `BotEvent`s onto a
//! broadcast channel. Audio dispatch is stubbed (`BotError::AudioNotImplemented`).
//!
//! No `tsclientlib::AudioHandler` here — WS-2 will plug audio in via a
//! sibling task that shares the same connection handle through a small
//! mutex / mpsc, decided in WS-2's design pass.
//!
//! Cleanroom rule applies: this file derives the bot loop from the
//! `tsclientlib` upstream API and the existing `ts6-voice-prototype`
//! event-handling pattern only.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;
use tracing::{debug, error, info, warn};
use tsclientlib::{
    events::Event as BookEvent, ChannelId as TsChannelId, ClientId, Connection,
    DisconnectOptions, MessageTarget, Reason, StreamItem,
};
// `OutCommandExt::send` is the dispatch sink for any `Out…Part` message
// produced by the generated book→messages helpers (`client_move`, etc.).
// The prelude re-exports it as `_` so the methods light up via glob.
use tsclientlib::prelude::*;

use ts6_voice_fixture::{load_or_create_identity, wait_for_connected};

use crate::backoff::ExponentialBackoff;
use crate::chat;
use crate::command::{AudioCommand, AudioSource, BotCommand, ChannelId, QueueCommand};
use crate::config::{BotConfig, BotId};
use crate::event::{BotError, BotEvent, DisconnectKind};
use crate::state::BotState;
use crate::store::{MusicBotStore, StoreError, Track};

/// Run the bot actor to completion. Exits when a `Shutdown` command has
/// been processed and the disconnect has flushed.
pub(crate) async fn run_bot(
    bot_id: BotId,
    config: BotConfig,
    store: Arc<dyn MusicBotStore>,
    mut rx: mpsc::Receiver<BotCommand>,
    events: broadcast::Sender<BotEvent>,
) {
    let span = tracing::info_span!("music_bot", bot_id = %bot_id, name = %config.name);
    let _enter = span.enter();
    info!("bot actor starting");

    let mut state = BotState::Disconnected;
    let mut backoff = ExponentialBackoff::new(config.backoff);
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
                        let _ = events.send(BotEvent::Error(BotError::Connection(format!(
                            "{err:#}"
                        ))));
                        if let Some(delay) = backoff.next_delay() {
                            info!(?delay, attempt = backoff.attempts(), "handshake retry sleep");
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

/// Drive the event stream + command queue while the bot is online.
/// Mirrors `ts6-voice-prototype`'s borrow-checker dance: build the events
/// future inline as the select arm so it gets dropped at each iteration,
/// freeing `&mut con` for command dispatch in the body.
async fn run_connected_loop(
    con: &mut Connection,
    state: &mut BotState,
    current_channel: &mut Option<ChannelId>,
    rx: &mut mpsc::Receiver<BotCommand>,
    events: &broadcast::Sender<BotEvent>,
    bot_id: BotId,
    store: &Arc<dyn MusicBotStore>,
) -> ConnectedExit {
    loop {
        tokio::select! {
            biased;
            ev = async { con.events().next().await } => match ev {
                Some(Ok(item)) => {
                    // PURA-122 WS-4 — pull any in-channel chat messages
                    // out of `BookEvents` BEFORE the channel-update logic
                    // consumes the item. Cheap because we only clone the
                    // event vector when chat is actually present.
                    let chat_msgs = extract_channel_chat(&item, con);
                    if let Some(channel) = handle_stream_item(item, con) {
                        if Some(channel) != *current_channel {
                            *current_channel = Some(channel);
                            transition(state, BotState::InChannel, events);
                            let _ = events.send(BotEvent::JoinedChannel { channel_id: channel });
                        }
                    }
                    for msg in chat_msgs {
                        dispatch_chat_line(con, bot_id, store, events, &msg).await;
                    }
                }
                Some(Err(err)) => {
                    return ConnectedExit::Dropped(format!("stream error: {err}"));
                }
                None => return ConnectedExit::Dropped("stream ended".into()),
            },
            cmd = rx.recv() => match cmd {
                Some(BotCommand::Disconnect) => return ConnectedExit::Disconnect,
                Some(BotCommand::Shutdown) => return ConnectedExit::Shutdown,
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
                Some(BotCommand::Audio(audio)) => {
                    // PURA-121 WS-3 — `SkipNext` is a queue advance even
                    // before WS-2 wires the audio task. Lower it here so
                    // the chat bridge (WS-4) and REST surface (WS-5) can
                    // exercise the auto-advance contract today.
                    if matches!(audio, AudioCommand::SkipNext) {
                        handle_queue_command(bot_id, store, QueueCommand::Advance, events).await;
                    } else {
                        emit_audio_stub(events, &audio);
                    }
                }
                Some(BotCommand::Queue(qc)) => {
                    handle_queue_command(bot_id, store, qc, events).await;
                }
                None => return ConnectedExit::Dropped("command channel closed".into()),
            },
        }
    }
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
async fn attempt_connect(
    config: &BotConfig,
) -> Result<(Connection, ClientId, ChannelId)> {
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
    let (own_id, default_channel) =
        wait_for_own_client(&mut con, Duration::from_secs(2)).await?;

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
    let label = match cmd {
        BotCommand::Connect => "Connect",
        BotCommand::Disconnect => "Disconnect",
        BotCommand::JoinChannel(_) => "JoinChannel",
        BotCommand::LeaveChannel => "LeaveChannel",
        BotCommand::Shutdown => "Shutdown",
        BotCommand::Audio(_) => "Audio",
        BotCommand::Queue(_) => "Queue",
    };
    warn!(command = label, ?state, "command rejected for current state");
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
            let was_empty = store.queue_peek(bot_id).await.map(|q| q.is_empty()).unwrap_or(false);
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
            let was_empty = store.queue_peek(bot_id).await.map(|q| q.is_empty()).unwrap_or(false);
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
            let head_before = store.queue_current(bot_id).await.ok().flatten().map(|t| t.id);
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
        QueueCommand::Advance => {
            match store.queue_dequeue_head(bot_id).await {
                Ok(_popped) => {
                    emit_queue_changed(store, bot_id, events).await;
                    emit_head_change(store, bot_id, events).await;
                }
                Err(err) => emit_store_error(events, "queue_dequeue_head", err),
            }
        }
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

fn emit_store_error(
    events: &broadcast::Sender<BotEvent>,
    op: &str,
    err: StoreError,
) {
    warn!(op, ?err, "store op failed");
    let _ = events.send(BotEvent::Error(BotError::Store {
        op: op.into(),
        message: err.to_string(),
    }));
}

fn emit_audio_stub(events: &broadcast::Sender<BotEvent>, cmd: &AudioCommand) {
    let label = match cmd {
        AudioCommand::Play { source } => match source {
            AudioSource::Url(u) => format!("Play(url:{u})"),
            AudioSource::LibraryPath(p) => format!("Play(library:{})", p.display()),
        },
        AudioCommand::Stop => "Stop".into(),
        AudioCommand::Pause => "Pause".into(),
        AudioCommand::Resume => "Resume".into(),
        AudioCommand::SkipNext => "SkipNext".into(),
        AudioCommand::SkipPrev => "SkipPrev".into(),
        AudioCommand::SetVolume(v) => format!("SetVolume({v})"),
        AudioCommand::NowPlaying(s) => format!("NowPlaying({s})"),
    };
    debug!(command = %label, "audio command — WS-2 will wire this");
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
async fn dispatch_chat_line(
    con: &mut Connection,
    bot_id: BotId,
    store: &Arc<dyn MusicBotStore>,
    events: &broadcast::Sender<BotEvent>,
    msg: &ChatLine,
) {
    match chat::parse(&msg.text) {
        Ok(parsed) => {
            debug!(invoker = %msg.invoker, ?parsed, "chat command");
            chat::dispatch(bot_id, con, store, events, parsed).await;
        }
        Err(err) => {
            debug!(invoker = %msg.invoker, line = %msg.text, ?err, "chat parse outcome");
            chat::reply_for_parse_error(con, &err);
        }
    }
}
