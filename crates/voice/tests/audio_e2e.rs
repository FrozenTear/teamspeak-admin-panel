//! PURA-154 — bot audio pipeline → `Connection::send_audio` → TS6 wire,
//! end-to-end against the local fixture.
//!
//! Sibling to `crates/ts6-voice-fixture/tests/audio_e2e.rs` (which proves
//! that raw `tsclientlib::Connection::send_audio` works against the
//! fixture). This test exercises the WS-1 actor + WS-2 pipeline together:
//!
//! 1. Spawn a receiver (raw `tsclientlib::Connection`) that collects S2C
//!    audio frames.
//! 2. Spawn a music-bot via `spawn_bot` with `auto_connect = true`.
//! 3. Send `BotCommand::Audio(Play { source: synthetic://... })`.
//! 4. Drain the receiver for ~2 s and assert that at least N non-empty
//!    Opus frames landed (drop tolerance mirrors the WS-2 audio_e2e — 5 %).
//! 5. Observe `BotEvent::AudioFinished` and shut down cleanly.
//!
//! Same gating discipline as `lifecycle_e2e.rs`: feature `audio-e2e` AND
//! env `TS6_VOICE_FIXTURE=1` AND `#[ignore]`. Workspace `cargo test`
//! never invokes it; the operator runs:
//!
//!     podman-compose --profile ts6-fixture up -d ts6-fixture
//!     TS6_VOICE_FIXTURE=1 cargo test -p music-bot \
//!         --features audio-e2e -- music_bot::audio_e2e \
//!         --ignored --nocapture

#![cfg(feature = "audio-e2e")]

extern crate music_bot as bot_lib;

use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bot_lib::{
    AudioCommand, AudioSource, BotCommand, BotConfig, BotEvent, BotId, InMemoryMusicBotStore,
    MusicBotStore, spawn_bot,
};
use futures::StreamExt;
use tokio::sync::broadcast;
use tokio::time::timeout;
use tracing::{info, warn};
use tsclientlib::{Connection, DisconnectOptions, Reason, StreamItem};
use tsproto_packets::packets::{AudioData, CodecType};

use ts6_voice_fixture::{load_or_create_identity, wait_for_connected};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
const EVENT_TIMEOUT: Duration = Duration::from_secs(20);
const DRAIN_WINDOW: Duration = Duration::from_secs(2);
/// Synthetic source emits a 500 ms tone at 20 ms cadence = 25 frames.
/// Allow the same 5 % drop budget the fixture test uses.
const EXPECTED_MIN_FRAMES: usize = 23;

mod music_bot {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore]
    async fn audio_e2e() {
        if !env_flag("TS6_VOICE_FIXTURE") {
            eprintln!(
                "[music_bot::audio_e2e] skipped — set TS6_VOICE_FIXTURE=1 \
                 after `podman-compose --profile ts6-fixture up -d ts6-fixture`. \
                 See docs/ts6-fixture.md §3."
            );
            return;
        }

        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                    "info,music_bot=debug,music_bot_audio=debug,tsclientlib=warn,tsproto=warn"
                        .into()
                }),
            )
            .with_test_writer()
            .try_init();

        if let Err(err) = run().await {
            eprintln!("\n=== music_bot::audio_e2e failed ===");
            for (i, cause) in err.chain().enumerate() {
                eprintln!("  [{i}] {cause}");
            }
            eprintln!("===================================\n");
            panic!("audio_e2e failed: {err:#}");
        }
    }
}

async fn run() -> Result<()> {
    let addr = env::var("TS6_VOICE_FIXTURE_ADDR").unwrap_or_else(|_| "127.0.0.1:9987".to_string());
    let workdir = std::env::temp_dir().join("music-bot-audio-e2e");
    let rx_dir = workdir.join("rx");
    let bot_dir = workdir.join("bot");

    // 1. Receiver — raw tsclientlib Connection. We don't need to send,
    //    just drain S2C frames the server forwards to in-channel
    //    listeners.
    let rx_identity = load_or_create_identity(&rx_dir.join("identity.json")).await?;
    let mut rx = Connection::build(addr.as_str())
        .name("qa-music-bot-audio-e2e-rx")
        .identity(rx_identity)
        .log_commands(false)
        .log_packets(false)
        .log_udp_packets(false)
        .connect()
        .context("build rx connection")?;
    if !wait_for_connected(&mut rx, HANDSHAKE_TIMEOUT)
        .await
        .context("rx handshake driver")?
    {
        bail!("rx never reached Connected within {HANDSHAKE_TIMEOUT:?}");
    }
    info!("receiver connected");

    // 2. Bot — uses the public `spawn_bot` API and auto-connects.
    let config = BotConfig::new("qa-music-bot-audio-e2e-bot", bot_dir.join("identity.json"))
        .with_server_addr(&addr)
        .with_handshake_timeout(HANDSHAKE_TIMEOUT)
        .with_auto_connect(true);
    let store: Arc<dyn MusicBotStore> = Arc::new(InMemoryMusicBotStore::new());
    let handle = spawn_bot(BotId(1), config, Arc::clone(&store));
    let mut bot_events = handle.subscribe();

    let (_, _) = match wait_for_bot_event(&mut bot_events, |ev| match ev {
        BotEvent::Connected {
            client_id,
            default_channel,
        } => Some((*client_id, *default_channel)),
        _ => None,
    })
    .await?
    {
        Some(pair) => pair,
        None => bail!("bot event stream ended before Connected"),
    };
    info!("bot connected");

    // Receiver collector runs concurrently while we drive the bot.
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let receiver = tokio::spawn(receiver_task(rx, stop_rx));

    // 3. Tell the bot to play a 500 ms synthetic tone. The `synthetic://`
    //    scheme is recognised by the WS-1 audio router and routes to the
    //    in-process SyntheticToneSource — no ffmpeg / yt-dlp involvement.
    let synth_url = "synthetic://?hz=440&duration_ms=500&amplitude=0.5";
    handle
        .send(BotCommand::Audio(AudioCommand::Play {
            source: AudioSource::Url(synth_url.into()),
        }))
        .await
        .context("dispatch Play")?;

    // 4. Wait for `AudioFinished` — the bot emits this after the pipeline
    //    drains AND the queue is empty (our direct Play bypasses the
    //    queue, so the post-EOF advance is a no-op).
    let finish_reason = match wait_for_bot_event(&mut bot_events, |ev| match ev {
        BotEvent::AudioFinished { reason } => Some(reason.clone()),
        _ => None,
    })
    .await?
    {
        Some(r) => r,
        None => bail!("bot event stream ended before AudioFinished"),
    };
    info!(reason = %finish_reason, "bot reported AudioFinished");

    // Drain a couple of seconds of in-flight UDP frames.
    tokio::time::sleep(DRAIN_WINDOW).await;
    let _ = stop_tx.send(());

    let recv_outcome = receiver
        .await
        .context("receiver task panicked")?
        .context("receiver task returned error")?;
    let nonempty = recv_outcome.frames.iter().filter(|f| !f.is_stop).count();
    let stops = recv_outcome.frames.iter().filter(|f| f.is_stop).count();
    info!(
        total = recv_outcome.frames.len(),
        nonempty, stops, "receiver summary"
    );

    if nonempty < EXPECTED_MIN_FRAMES {
        bail!(
            "received only {nonempty} non-empty Opus frames; expected ≥ {EXPECTED_MIN_FRAMES} \
             (25 sent, 5 % drop budget). Likely cause: fixture's Default Channel Guest group \
             lacks `b_channel_voice_speak`. See `crates/ts6-voice-fixture/tests/audio_e2e.rs` \
             for the privilege-key recovery procedure."
        );
    }
    if stops == 0 {
        bail!(
            "no voice-stop frame observed — the bot should send an empty Opus payload after \
             pipeline EOF (see `audio::send_voice_stop`)."
        );
    }

    // 5. Clean shutdown.
    handle.shutdown().await.context("BotHandle::shutdown")?;
    info!("audio_e2e passed");
    Ok(())
}

#[derive(Debug, Clone)]
struct ReceivedFrame {
    is_stop: bool,
    #[allow(dead_code)]
    codec: u8,
}

struct ReceiverOutcome {
    frames: Vec<ReceivedFrame>,
}

async fn receiver_task(
    mut con: Connection,
    mut stop: tokio::sync::oneshot::Receiver<()>,
) -> Result<ReceiverOutcome> {
    let mut frames = Vec::with_capacity(256);
    {
        let events = con.events();
        tokio::pin!(events);
        loop {
            tokio::select! {
                biased;
                _ = &mut stop => break,
                ev = events.next() => match ev {
                    Some(Ok(StreamItem::Audio(packet))) => {
                        let data = packet.data().data();
                        let (codec, opus): (CodecType, &[u8]) = match data {
                            AudioData::S2C { codec, data, .. } => (*codec, *data),
                            AudioData::S2CWhisper { codec, data, .. } => (*codec, *data),
                            _ => continue,
                        };
                        if !matches!(codec, CodecType::OpusVoice | CodecType::OpusMusic) {
                            continue;
                        }
                        frames.push(ReceivedFrame {
                            is_stop: opus.is_empty(),
                            codec: codec as u8,
                        });
                    }
                    Some(Ok(_)) => { /* book / disconnect-temporary / etc */ }
                    Some(Err(err)) => {
                        warn!(?err, "receiver stream error");
                        break;
                    }
                    None => break,
                }
            }
        }
    }
    if let Err(err) = con.disconnect(
        DisconnectOptions::new()
            .reason(Reason::Clientdisconnect)
            .message("music-bot audio_e2e rx done"),
    ) {
        warn!(?err, "rx disconnect failed (non-fatal)");
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        let drain = con.events();
        tokio::pin!(drain);
        while drain.next().await.is_some() {}
    })
    .await;
    Ok(ReceiverOutcome { frames })
}

async fn wait_for_bot_event<T, F>(
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
            bail!("timed out waiting for bot event match (deadline {EVENT_TIMEOUT:?})");
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
            Err(_) => bail!("timed out waiting for bot event match (deadline {EVENT_TIMEOUT:?})"),
        }
    }
}

fn env_flag(name: &str) -> bool {
    matches!(env::var(name).ok().as_deref(), Some("1") | Some("true"))
}
