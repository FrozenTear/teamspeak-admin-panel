//! PURA-122 WS-4 — chat-bridge end-to-end against the local TS6 fixture.
//!
//! Spawns the music-bot (the "subject under test") plus a raw
//! `tsclientlib::Connection` (the "operator") into the same channel,
//! sends real `!`-commands as TS6 channel chat from the operator, and
//! asserts that:
//!
//! - the music-bot's `BotEvent` broadcast reflects the dispatched command
//!   (queue mutated, now-playing fired);
//! - the music-bot replied with a single short channel-chat line that the
//!   operator can read off its own connection's event stream.
//!
//! Acceptance bar mirrors the WS-1 lifecycle e2e: feature-gated AND
//! env-gated AND `#[ignore]` so workspace `cargo test` never tries to run
//! it without an explicit operator opt-in.
//!
//! Run locally:
//!
//!     podman-compose --profile ts6-fixture up -d ts6-fixture
//!     TS6_VOICE_FIXTURE=1 cargo test -p music-bot \
//!         --features lifecycle-e2e -- music_bot::chat_bridge_e2e \
//!         --ignored --nocapture
//!
//! Cleanroom rule: derived from the music-bot crate's public surface and
//! the `tsclientlib` upstream API only.

#![cfg(feature = "lifecycle-e2e")]

extern crate music_bot as bot_lib;

use std::env;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bot_lib::{BotConfig, BotEvent, BotId, InMemoryMusicBotStore, MusicBotStore, spawn_bot};
use futures::StreamExt;
use tokio::sync::broadcast;
use tokio::time::{Instant, timeout};
use tsclientlib::prelude::*;
use tsclientlib::{
    ChannelId as TsChannelId, Connection, DisconnectOptions, MessageTarget, Reason, StreamItem,
    events::Event as BookEvent,
};

use ts6_voice_fixture::{load_or_create_identity, wait_for_connected};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
const EVENT_TIMEOUT: Duration = Duration::from_secs(15);

mod music_bot {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore]
    async fn chat_bridge_e2e() {
        if !env_flag("TS6_VOICE_FIXTURE") {
            eprintln!(
                "[music_bot::chat_bridge_e2e] skipped — set TS6_VOICE_FIXTURE=1 \
                 after `podman-compose --profile ts6-fixture up -d ts6-fixture`. \
                 See docs/ts6-fixture.md §3."
            );
            return;
        }

        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                    "info,music_bot=debug,tsclientlib=warn,tsproto=warn".into()
                }),
            )
            .with_test_writer()
            .try_init();

        if let Err(err) = run().await {
            eprintln!("\n=== music_bot::chat_bridge_e2e failed ===");
            for (i, cause) in err.chain().enumerate() {
                eprintln!("  [{i}] {cause}");
            }
            eprintln!("==========================================\n");
            panic!("chat_bridge_e2e failed: {err:#}");
        }
    }
}

async fn run() -> Result<()> {
    let addr = env::var("TS6_VOICE_FIXTURE_ADDR").unwrap_or_else(|_| "127.0.0.1:9987".to_string());
    let workdir = std::env::temp_dir().join("music-bot-chat-bridge-e2e");
    let bot_identity = workdir.join("bot-identity.json");
    let op_identity = workdir.join("operator-identity.json");

    // 1. Spawn the music-bot.
    let bot_config = BotConfig::new("qa-music-bot", &bot_identity)
        .with_server_addr(&addr)
        .with_handshake_timeout(HANDSHAKE_TIMEOUT)
        .with_auto_connect(true);
    let store: Arc<dyn MusicBotStore> = Arc::new(InMemoryMusicBotStore::new());
    let bot = spawn_bot(
        BotId(1, std::sync::Arc::new(std::sync::RwLock::new(None))),
        bot_config,
        store,
    );
    let mut bot_events = bot.subscribe();

    // 2. Wait for the bot's `Connected` event so we know the default channel.
    let (bot_client_id, bot_channel) = wait_for_event(&mut bot_events, |ev| match ev {
        BotEvent::Connected {
            client_id,
            default_channel,
        } => Some((*client_id, *default_channel)),
        _ => None,
    })
    .await
    .context("waiting for bot Connected")?
    .ok_or_else(|| anyhow::anyhow!("bot event stream ended before Connected"))?;
    eprintln!("bot connected: client={bot_client_id} channel={bot_channel}");

    // 3. Spin up the operator connection.
    let operator_identity = load_or_create_identity(&op_identity)
        .await
        .context("operator identity")?;
    let mut operator = Connection::build(addr.as_str())
        .name("qa-operator".to_string())
        .identity(operator_identity)
        .log_commands(false)
        .log_packets(false)
        .log_udp_packets(false)
        .connect()
        .context("operator Connection::build()")?;
    let connected = wait_for_connected(&mut operator, HANDSHAKE_TIMEOUT)
        .await
        .context("operator handshake driver")?;
    if !connected {
        bail!("operator handshake did not complete in {HANDSHAKE_TIMEOUT:?}");
    }
    // Drain a beat so the operator's own client appears in its book.
    drain_for(&mut operator, Duration::from_millis(500)).await;

    // 4. Move the operator into the bot's channel.
    move_to_channel(&mut operator, bot_channel).context("operator channel-move")?;
    // Wait until the operator's book confirms it's in the right channel
    // AND the bot is visible to the operator in that channel — otherwise
    // channel chat won't propagate yet.
    wait_until_co_located(&mut operator, bot_channel, bot_client_id)
        .await
        .context("operator + bot co-location")?;

    // 5. `!np` against an empty queue → bot replies "queue is empty".
    send_chat(&mut operator, "!np").context("send !np")?;
    let np_reply = wait_for_chat_reply(&mut operator)
        .await
        .context("waiting for !np reply")?;
    if !np_reply.contains("queue is empty") {
        bail!("unexpected !np reply: {np_reply:?}");
    }
    eprintln!("!np reply: {np_reply}");

    // 6. `!play <url>` enqueues + fires NowPlaying + bot replies with playing:
    send_chat(&mut operator, "!play https://example.com/test.mp3").context("send !play")?;

    // bot side: QueueChanged then NowPlaying.
    let queue_evt = wait_for_event(&mut bot_events, |ev| match ev {
        BotEvent::QueueChanged { len, current } => Some((*len, current.is_some())),
        _ => None,
    })
    .await
    .context("waiting for QueueChanged after !play")?
    .ok_or_else(|| anyhow::anyhow!("bot stream ended before QueueChanged"))?;
    if queue_evt != (1, true) {
        bail!(
            "unexpected QueueChanged after !play: len={} has_current={}",
            queue_evt.0,
            queue_evt.1
        );
    }
    let now_playing = wait_for_event(&mut bot_events, |ev| match ev {
        BotEvent::NowPlaying(t) => Some(t.title.clone()),
        _ => None,
    })
    .await
    .context("waiting for NowPlaying after !play")?
    .ok_or_else(|| anyhow::anyhow!("bot stream ended before NowPlaying"))?;
    eprintln!("NowPlaying title: {now_playing}");

    // operator side: bot replied with "playing: …".
    let play_reply = wait_for_chat_reply(&mut operator)
        .await
        .context("waiting for !play reply")?;
    if !play_reply.starts_with("playing:") {
        bail!("unexpected !play reply: {play_reply:?}");
    }
    eprintln!("!play reply: {play_reply}");

    // 7. `!stop` → queue cleared + reply "stopped".
    send_chat(&mut operator, "!stop").context("send !stop")?;
    let stop_evt = wait_for_event(&mut bot_events, |ev| match ev {
        BotEvent::QueueChanged {
            len: 0,
            current: None,
        } => Some(()),
        _ => None,
    })
    .await
    .context("waiting for QueueChanged(empty) after !stop")?
    .ok_or_else(|| anyhow::anyhow!("bot stream ended before stop"))?;
    let _ = stop_evt;
    let stop_reply = wait_for_chat_reply(&mut operator)
        .await
        .context("waiting for !stop reply")?;
    if !stop_reply.contains("stopped") {
        bail!("unexpected !stop reply: {stop_reply:?}");
    }
    eprintln!("!stop reply: {stop_reply}");

    // 8. Cleanup — disconnect operator and shut down the bot.
    let _ = operator.disconnect(
        DisconnectOptions::new()
            .reason(Reason::Clientdisconnect)
            .message("test done".to_string()),
    );
    drain_for(&mut operator, Duration::from_millis(250)).await;
    bot.shutdown().await.context("BotHandle::shutdown")?;
    Ok(())
}

// -----------------------------------------------------------------------
// helpers
// -----------------------------------------------------------------------

fn move_to_channel(con: &mut Connection, target: u64) -> Result<()> {
    let book = con.get_state().context("operator book")?;
    let own = book
        .clients
        .get(&book.own_client)
        .context("operator own client not in book")?;
    own.client_move(TsChannelId(target))
        .send(con)
        .context("OutCommandExt::send (operator client-move)")
}

fn send_chat(con: &mut Connection, text: &str) -> Result<()> {
    let cmd = {
        let book = con.get_state().context("operator book")?;
        book.send_message(MessageTarget::Channel, text)
    };
    cmd.send(con).context("OutCommandExt::send (operator chat)")
}

/// Drive the operator's event stream until either (a) it's in `target`
/// channel AND the bot's `client_id` is also visible there, or (b) the
/// outer timeout fires.
async fn wait_until_co_located(
    con: &mut Connection,
    target: u64,
    bot_client_id: u16,
) -> Result<()> {
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        // Check the book first so we don't block on the next event when
        // the move already landed.
        if let Ok(book) = con.get_state() {
            let own = book.clients.get(&book.own_client);
            let bot = book.clients.iter().find(|(id, _)| id.0 == bot_client_id);
            if let (Some(own), Some((_, bot_client))) = (own, bot) {
                if own.channel.0 == target && bot_client.channel.0 == target {
                    return Ok(());
                }
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("co-location wait timed out (target={target}, bot_client={bot_client_id})");
        }
        tokio::select! {
            biased;
            _ = tokio::time::sleep(remaining) => {
                bail!("co-location wait timed out (no events)");
            }
            ev = async { con.events().next().await } => match ev {
                Some(Ok(_)) => continue,
                Some(Err(err)) => bail!("operator stream error: {err}"),
                None => bail!("operator stream ended before co-location"),
            }
        }
    }
}

/// Drain the operator's event stream for at most `dur` so book
/// notifications settle. Used after disconnect / immediately after
/// connect.
async fn drain_for(con: &mut Connection, dur: Duration) {
    let deadline = Instant::now() + dur;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match timeout(remaining, async { con.events().next().await }).await {
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(_) => break,
        }
    }
}

/// Wait for the next channel-chat reply on the operator's connection.
/// Filters out chat from the operator itself (defensive — we shouldn't
/// receive our own send-back, but TS6 echo behaviour varies by version).
async fn wait_for_chat_reply(con: &mut Connection) -> Result<String> {
    let own_id = con.get_state().context("operator book")?.own_client;
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("timed out waiting for chat reply");
        }
        let ev = match timeout(remaining, async { con.events().next().await }).await {
            Ok(Some(Ok(item))) => item,
            Ok(Some(Err(err))) => bail!("operator stream error: {err}"),
            Ok(None) => bail!("operator stream ended before reply"),
            Err(_) => bail!("timed out waiting for chat reply"),
        };
        if let StreamItem::BookEvents(events) = ev {
            for e in events {
                if let BookEvent::Message {
                    target: MessageTarget::Channel,
                    invoker,
                    message,
                } = e
                {
                    if invoker.id == own_id {
                        continue;
                    }
                    return Ok(message);
                }
            }
        }
    }
}

async fn wait_for_event<T, F>(
    rx: &mut broadcast::Receiver<BotEvent>,
    mut pred: F,
) -> Result<Option<T>>
where
    F: FnMut(&BotEvent) -> Option<T>,
{
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("timed out waiting for event match (deadline {EVENT_TIMEOUT:?})");
        }
        match timeout(remaining, rx.recv()).await {
            Ok(Ok(ev)) => {
                eprintln!("  bot event: {ev:?}");
                if let Some(t) = pred(&ev) {
                    return Ok(Some(t));
                }
            }
            Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                eprintln!("  (lagged {n} bot events)");
                continue;
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => return Ok(None),
            Err(_) => bail!("timed out waiting for event match"),
        }
    }
}

fn env_flag(name: &str) -> bool {
    matches!(env::var(name).ok().as_deref(), Some("1") | Some("true"))
}
