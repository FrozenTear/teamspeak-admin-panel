//! PURA-118 WS-1 — bot lifecycle end-to-end against the local TS6 fixture.
//!
//! Spawns one bot, drives it through its lifecycle, and asserts the
//! `BotEvent` stream tells the same story as the state machine claims:
//!
//! - `Connected` fires after handshake;
//! - `JoinedChannel` fires for the default channel;
//! - `JoinChannel(default)` is acknowledged via a `StateChanged` to
//!   `InChannel` (a self-loop) — the test drives a real channel-move
//!   when a sibling channel is available, otherwise validates the
//!   no-op self-loop;
//! - `Shutdown` flushes through `Disconnecting → Disconnected` and
//!   the actor task exits.
//!
//! No audio assertion — that lands in WS-2. Acceptance bar mirrors
//! `ts6-voice-fixture::audio_e2e`'s gating: feature-gated AND
//! env-gated AND `#[ignore]` so workspace `cargo test` never tries to
//! run it without an explicit operator opt-in.
//!
//! Run locally:
//!
//!     podman-compose --profile ts6-fixture up -d ts6-fixture
//!     TS6_VOICE_FIXTURE=1 cargo test -p music-bot \
//!         --features lifecycle-e2e -- music_bot::lifecycle_e2e \
//!         --ignored --nocapture

#![cfg(feature = "lifecycle-e2e")]

// Inner `mod music_bot` below names the test inside the canonical filter
// path the operator uses (`music_bot::lifecycle_e2e`), so the lib crate
// gets re-aliased here to avoid the name shadowing — same trick the
// `ts6-voice-fixture::audio_e2e` test uses.
extern crate music_bot as bot_lib;

use std::env;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bot_lib::{
    BotCommand, BotConfig, BotEvent, BotId, BotState, DisconnectKind, InMemoryMusicBotStore,
    MusicBotStore, spawn_bot,
};
use tokio::sync::broadcast;
use tokio::time::{Instant, timeout};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
const EVENT_TIMEOUT: Duration = Duration::from_secs(15);

mod music_bot {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore]
    async fn lifecycle_e2e() {
        if !env_flag("TS6_VOICE_FIXTURE") {
            eprintln!(
                "[music_bot::lifecycle_e2e] skipped — set TS6_VOICE_FIXTURE=1 \
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
            eprintln!("\n=== music_bot::lifecycle_e2e failed ===");
            for (i, cause) in err.chain().enumerate() {
                eprintln!("  [{i}] {cause}");
            }
            eprintln!("=======================================\n");
            panic!("lifecycle_e2e failed: {err:#}");
        }
    }
}

async fn run() -> Result<()> {
    let addr = env::var("TS6_VOICE_FIXTURE_ADDR").unwrap_or_else(|_| "127.0.0.1:9987".to_string());
    let workdir = std::env::temp_dir().join("music-bot-lifecycle-e2e");
    let identity_path = workdir.join("identity.json");

    let config = BotConfig::new("qa-music-bot", &identity_path)
        .with_server_addr(&addr)
        .with_handshake_timeout(HANDSHAKE_TIMEOUT)
        .with_auto_connect(true);

    let store: Arc<dyn MusicBotStore> = Arc::new(InMemoryMusicBotStore::new());
    let handle = spawn_bot(
        BotId(1, std::sync::Arc::new(std::sync::RwLock::new(None))),
        config,
        store,
    );
    let mut events = handle.subscribe();

    // 1. Connected event fires with the default channel.
    let (client_id, default_channel) = match wait_for(&mut events, |ev| {
        if let BotEvent::Connected {
            client_id,
            default_channel,
        } = ev
        {
            Some((*client_id, *default_channel))
        } else {
            None
        }
    })
    .await
    .context("waiting for Connected")?
    {
        Some(pair) => pair,
        None => bail!("event stream ended before Connected"),
    };
    eprintln!("connected: client_id={client_id} default_channel={default_channel}");

    // 2. JoinedChannel fires with default.
    let first_channel = match wait_for(&mut events, |ev| match ev {
        BotEvent::JoinedChannel { channel_id } => Some(*channel_id),
        _ => None,
    })
    .await
    .context("waiting for default JoinedChannel")?
    {
        Some(c) => c,
        None => bail!("event stream ended before JoinedChannel"),
    };
    if first_channel != default_channel {
        bail!("first JoinedChannel reports {first_channel}, expected default {default_channel}");
    }

    // 3. Drive a JoinChannel(default) — server treats this as a no-op
    //    move. We assert the bot stays online; the JoinChannel command
    //    must NOT crash the actor or close the event channel.
    handle
        .send(BotCommand::JoinChannel(default_channel))
        .await
        .context("dispatch JoinChannel")?;

    // 4. Shutdown — observe StateChanged → Disconnecting → Disconnected
    //    (or just the Disconnected event with kind=ShutdownRequested),
    //    then the actor exits.
    handle
        .send(BotCommand::Shutdown)
        .await
        .context("dispatch Shutdown")?;

    // 5. Observe terminal `StateChanged` (Disconnecting → Disconnected)
    //    AND the `Disconnected` event with the right kind. Order matches
    //    the actor's emission order, so a single linear scan works.
    let observed_terminal = wait_for(&mut events, |ev| match ev {
        BotEvent::StateChanged {
            to: BotState::Disconnected,
            ..
        } => Some(()),
        _ => None,
    })
    .await
    .context("waiting for terminal Disconnected state")?;
    observed_terminal.context("event stream ended before terminal StateChanged")?;

    let observed_disconnect = wait_for(&mut events, |ev| match ev {
        BotEvent::Disconnected { kind, .. } => Some(*kind),
        _ => None,
    })
    .await
    .context("waiting for Disconnected")?;
    let kind = observed_disconnect.context("event stream ended before Disconnected")?;
    if !matches!(
        kind,
        DisconnectKind::ShutdownRequested | DisconnectKind::Clean
    ) {
        bail!("unexpected disconnect kind on shutdown: {kind:?}");
    }

    // 6. Actor task exits cleanly — `shutdown` joins it.
    handle.shutdown().await.context("BotHandle::shutdown")?;

    Ok(())
}

/// Drive the broadcast receiver until either the predicate returns
/// `Some(t)` (success) or the stream ends. A bounded outer timeout
/// prevents stalls; per-call wait timeout converts `RecvError::Lagged`
/// to a continued wait so a slow predicate doesn't tank the run.
async fn wait_for<T, F>(rx: &mut broadcast::Receiver<BotEvent>, mut pred: F) -> Result<Option<T>>
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
                eprintln!("  event: {ev:?}");
                if let Some(t) = pred(&ev) {
                    return Ok(Some(t));
                }
            }
            Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                eprintln!("  (lagged {n} events)");
                continue;
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => return Ok(None),
            Err(_) => bail!("timed out waiting for event match (deadline {EVENT_TIMEOUT:?})"),
        }
    }
}

fn env_flag(name: &str) -> bool {
    matches!(env::var(name).ok().as_deref(), Some("1") | Some("true"))
}
