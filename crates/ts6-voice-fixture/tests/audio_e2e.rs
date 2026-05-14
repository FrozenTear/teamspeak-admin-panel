//! PURA-110 — audio frames flow end-to-end through the local TS6 fixture.
//!
//! Extends PURA-106 (connect-only) by spawning a sender + a receiver against
//! the self-hosted `teamspeaksystems/teamspeak6-server` fixture and asserting:
//!
//! - frame count: receiver got ≥(1 - drop_tol) × frames sent (default 5%);
//! - codec continuity: every received S2C frame uses the codec the sender
//!   used (default `OpusVoice` = 4);
//! - voice-stop: the receiver sees at least one final empty-payload frame.
//!
//! Env-gated. Compiled only with `--features ts6-voice-fixture` (so the
//! audiopus + tsproto-packets deps stay out of the default workspace build),
//! and skipped at runtime unless `TS6_VOICE_FIXTURE=1`. The `#[ignore]`
//! attribute is the second seatbelt — `cargo test --workspace` ignores it
//! even when the feature happens to be on. Run locally with:
//!
//!     podman-compose --profile ts6-fixture up -d ts6-fixture
//!     TS6_VOICE_FIXTURE=1 cargo test -p ts6-voice-fixture \
//!         --features ts6-voice-fixture -- ts6_voice_fixture::audio_e2e \
//!         --ignored --nocapture
//!
//! Continuation of the PURA-7 Day-2 voice-tx spike. Body layout:
//! `voice_id u16 BE | codec_id u8 | opus_payload`.

#![cfg(feature = "ts6-voice-fixture")]

// Rename the lib crate locally so the inner `mod ts6_voice_fixture` (which
// gives the test the canonical filter path the ticket asks for) doesn't
// shadow it.
extern crate ts6_voice_fixture as voice_lib;

use std::env;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use audiopus::{Application, Channels, SampleRate, coder::Encoder};
use futures::{StreamExt, TryStreamExt};
use tokio::sync::oneshot;
use tokio::time::interval;
use tracing::{info, warn};
use tsclientlib::{AudioEvent, Connection, DisconnectOptions, Reason, StreamItem};
use tsproto_packets::packets::{AudioData, CodecType, OutAudio};
use voice_lib::{load_or_create_identity, wait_for_connected};

const FRAME_SAMPLES: usize = 48_000 / 50; // 20 ms @ 48 kHz mono = 960
const MAX_OPUS_FRAME: usize = 4_000;
const FRAME_INTERVAL_MS: u64 = 20;
const DEFAULT_FRAMES: u32 = 1_500; // 30 s @ 20 ms — matches PURA-110 deliverable.
const DEFAULT_DROP_TOL: f64 = 0.05; // 5 % drop budget (UDP, no FEC).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// Snapshot of one S2C audio frame the receiver pulled off the wire.
/// The `voice_id`, `from`, and `payload_len` fields are read only via the
/// `Debug` derive in the codec-mismatch diagnostic — silence the dead-code
/// lint since they are deliberately kept for failure-path forensics.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct ReceivedFrame {
    voice_id: u16,
    from: u16,
    codec: u8,
    payload_len: usize,
    /// `true` when the opus payload is empty — the spec's voice-stop signal.
    is_stop: bool,
}

// Wrapped in a module so the canonical PURA-110 filter
// `ts6_voice_fixture::audio_e2e` from the ticket text resolves to this fn.
// Cargo test path = `<integration-test-file>::<module-path>::<fn>`, so the
// filename `audio_e2e.rs` plus this `ts6_voice_fixture` mod gives us
// `audio_e2e::ts6_voice_fixture::audio_e2e` — the string `ts6_voice_fixture::audio_e2e`
// is a substring filter that matches uniquely.
mod ts6_voice_fixture {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore]
    async fn audio_e2e() {
        if !env_flag("TS6_VOICE_FIXTURE") {
            eprintln!(
                "[ts6_voice_fixture::audio_e2e] skipped — set TS6_VOICE_FIXTURE=1 \
                 after `podman-compose --profile ts6-fixture up -d ts6-fixture`. \
                 See docs/ts6-fixture.md §3."
            );
            return;
        }

        if let Err(err) = run_audio_e2e().await {
            // Print the full chain so the operator sees the *cause* — anyhow's
            // Display only renders the leaf. The wedged-fixture path goes through
            // here too (handshake timeout, `Connection refused`, etc.).
            eprintln!("\n=== ts6_voice_fixture::audio_e2e failed ===");
            for (i, cause) in err.chain().enumerate() {
                eprintln!("  [{i}] {cause}");
            }
            eprintln!("===========================================\n");
            panic!("audio_e2e failed: {err:#}");
        }
    }
}

async fn run_audio_e2e() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "info,ts6_voice_fixture=debug,tsclientlib=warn,tsproto=warn".into()
            }),
        )
        .with_test_writer()
        .try_init();

    let addr = env::var("TS6_VOICE_FIXTURE_ADDR").unwrap_or_else(|_| "127.0.0.1:9987".to_string());
    let frames_to_send = env_u32("TS6_VOICE_FIXTURE_FRAMES", DEFAULT_FRAMES);
    let drop_tol = env_f64("TS6_VOICE_FIXTURE_DROP_TOL", DEFAULT_DROP_TOL);
    let codec = CodecType::OpusVoice;
    let codec_byte = codec as u8;

    info!(addr = %addr, frames_to_send, drop_tol, "audio E2E starting");

    // Distinct identity dirs for the two participants — the binary fixture's
    // single-instance lock is per-dir, and we want both to coexist.
    let workdir = std::env::temp_dir().join("ts6-voice-fixture-audio-e2e");
    let tx_dir = workdir.join("tx");
    let rx_dir = workdir.join("rx");
    let tx_identity = load_or_create_identity(&tx_dir.join("identity.json")).await?;
    let rx_identity = load_or_create_identity(&rx_dir.join("identity.json")).await?;

    let mut tx = Connection::build(addr.as_str())
        .name("qa-fixture-tx")
        .identity(tx_identity)
        .log_commands(false)
        .log_packets(false)
        .log_udp_packets(false)
        .connect()
        .context("build tx connection")?;
    let mut rx = Connection::build(addr.as_str())
        .name("qa-fixture-rx")
        .identity(rx_identity)
        .log_commands(false)
        .log_packets(false)
        .log_udp_packets(false)
        .connect()
        .context("build rx connection")?;

    // Both clients must reach `BookEvents` before we can send/receive. Drive
    // them in parallel — the second handshake doesn't need to wait for the
    // first.
    let (tx_handshake, rx_handshake) = tokio::join!(
        wait_for_connected(&mut tx, HANDSHAKE_TIMEOUT),
        wait_for_connected(&mut rx, HANDSHAKE_TIMEOUT),
    );
    if !tx_handshake.context("tx handshake driver")? {
        wedged_fixture_bail("tx never reached Connected", &addr)?;
    }
    if !rx_handshake.context("rx handshake driver")? {
        wedged_fixture_bail("rx never reached Connected", &addr)?;
    }

    info!("both clients connected — starting send/receive tasks");

    // Stop signal: receiver runs until either the sender pings it or the
    // global wall-clock budget expires.
    let (stop_tx, stop_rx) = oneshot::channel::<()>();

    // Receiver task — collects all incoming S2C audio frames into a Vec.
    // Holds the full Connection so it can keep the stream pumped; we move it
    // back out at the end to disconnect cleanly.
    let receiver = tokio::spawn(receiver_task(rx, stop_rx));

    // Sender task — produces `frames_to_send` Opus frames at 20 ms cadence
    // and a final empty stop frame, then disconnects. Returns the count
    // actually sent (== frames_to_send unless the stream errored).
    let sender = tokio::spawn(sender_task(tx, codec, frames_to_send));

    let sent = sender
        .await
        .context("sender task panicked")?
        .context("sender task returned error")?;

    info!(sent, "sender finished — draining receiver for 1 s");
    // Let the last in-flight UDP frames land before we cut the receiver.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let _ = stop_tx.send(());

    let receiver_outcome = receiver
        .await
        .context("receiver task panicked")?
        .context("receiver task returned error")?;

    // Assertions ----------------------------------------------------------

    let received_total = receiver_outcome.frames.len();
    let nonempty = receiver_outcome
        .frames
        .iter()
        .filter(|f| !f.is_stop)
        .count();
    let stops = receiver_outcome.frames.iter().filter(|f| f.is_stop).count();
    let codec_mismatches: Vec<&ReceivedFrame> = receiver_outcome
        .frames
        .iter()
        .filter(|f| f.codec != codec_byte)
        .collect();

    info!(
        sent,
        received_total,
        nonempty,
        stops,
        rx_can_send = receiver_outcome.last_can_send_audio,
        rx_can_receive = receiver_outcome.last_can_receive_audio,
        "receive summary",
    );

    // 1. Frame count vs sent (within drop tolerance).
    let min_expected = ((sent as f64) * (1.0 - drop_tol)).floor() as usize;
    if nonempty < min_expected {
        bail!(
            "frame drop above tolerance: sent={sent}, received non-empty={nonempty}, \
             min expected={min_expected} (drop_tol={drop_tol:.3}). \
             rx_can_send_audio={:?}, rx_can_receive_audio={:?}. \
             Likely cause: the fixture's default Guest server-group lacks \
             `b_channel_voice_speak` in the Default Channel. Claim ServerAdmin \
             via the fixture's privilege key and grant Guest voice, then re-run.",
            receiver_outcome.last_can_send_audio,
            receiver_outcome.last_can_receive_audio,
        );
    }

    // 2. Codec-id continuity.
    if !codec_mismatches.is_empty() {
        bail!(
            "codec-id mismatch: expected {codec_byte} ({codec:?}) on every frame, \
             got {} mismatches; first = {:?}",
            codec_mismatches.len(),
            codec_mismatches.first(),
        );
    }

    // 3. Voice-stop signalling.
    if stops == 0 {
        bail!(
            "no voice-stop frame observed (empty Opus payload). The sender \
             dispatched one — either the server suppressed it or the receiver \
             disconnected before it landed."
        );
    }

    info!(
        "PASS — sent={sent}, received non-empty={nonempty} \
         (≥ {min_expected}), stops={stops}, codec_byte={codec_byte}",
    );
    Ok(())
}

struct ReceiverOutcome {
    frames: Vec<ReceivedFrame>,
    last_can_send_audio: Option<bool>,
    last_can_receive_audio: Option<bool>,
}

async fn receiver_task(
    mut con: Connection,
    mut stop: oneshot::Receiver<()>,
) -> Result<ReceiverOutcome> {
    let mut frames = Vec::with_capacity(2048);
    let mut last_can_send_audio: Option<bool> = None;
    let mut last_can_receive_audio: Option<bool> = None;

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
                        match data {
                            AudioData::S2C { id, from, codec, data } => {
                                frames.push(ReceivedFrame {
                                    voice_id: *id,
                                    from: *from,
                                    codec: *codec as u8,
                                    payload_len: data.len(),
                                    is_stop: data.is_empty(),
                                });
                            }
                            AudioData::S2CWhisper { id, from, codec, data } => {
                                frames.push(ReceivedFrame {
                                    voice_id: *id,
                                    from: *from,
                                    codec: *codec as u8,
                                    payload_len: data.len(),
                                    is_stop: data.is_empty(),
                                });
                            }
                            other => {
                                warn!(?other, "unexpected non-S2C audio data on receiver");
                            }
                        }
                    }
                    Some(Ok(StreamItem::AudioChange(AudioEvent::CanSendAudio(v)))) => {
                        last_can_send_audio = Some(v);
                    }
                    Some(Ok(StreamItem::AudioChange(AudioEvent::CanReceiveAudio(v)))) => {
                        last_can_receive_audio = Some(v);
                    }
                    Some(Ok(_)) => { /* book / disconnect-temporary / etc. */ }
                    Some(Err(err)) => {
                        warn!(?err, "receiver stream error");
                        break;
                    }
                    None => break,
                }
            }
        }
    }

    // Best-effort clean disconnect — failure here doesn't fail the test.
    if let Err(err) = con.disconnect(
        DisconnectOptions::new()
            .reason(Reason::Clientdisconnect)
            .message("audio_e2e rx done"),
    ) {
        warn!(?err, "rx disconnect call failed");
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        let drain = con.events();
        tokio::pin!(drain);
        while drain.next().await.is_some() {}
    })
    .await;

    Ok(ReceiverOutcome {
        frames,
        last_can_send_audio,
        last_can_receive_audio,
    })
}

async fn sender_task(mut con: Connection, codec: CodecType, frames: u32) -> Result<u32> {
    // 20 ms of a 440 Hz mono sine — every frame encodes the same payload.
    // Steady tone is nicer for the operator to spot in a Wireshark capture
    // than silence, and is identical bytes so the encoder is a one-shot.
    let pcm = synth_sine(440.0, FRAME_SAMPLES);
    let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)
        .context("Opus encoder init")?;
    let mut opus_buf = vec![0u8; MAX_OPUS_FRAME];
    let opus_len = encoder
        .encode_float(&pcm, &mut opus_buf)
        .context("encode 20 ms Opus frame")?;
    info!(opus_len, "encoded reference Opus frame");

    let mut tick = interval(Duration::from_millis(FRAME_INTERVAL_MS));
    let mut sent: u32 = 0;
    let started = Instant::now();

    // Pattern lifted from PURA-7 Day-2 voice-tx spike: collapse the event
    // stream into a `try_for_each` *Future* (not a Stream) so `tokio::select!`
    // owns it and drops it cleanly each iteration. That releases the &mut con
    // borrow before the tick arm calls `con.send_audio(...)`.
    while sent < frames {
        let events = con.events().try_for_each(|_e| async { Ok(()) });
        tokio::select! {
            biased;
            _ = tick.tick() => {
                let pkt = OutAudio::new(&AudioData::C2S {
                    id: 0,
                    codec,
                    data: &opus_buf[..opus_len],
                });
                con.send_audio(pkt).context("send_audio voice frame")?;
                sent += 1;
                if sent % 250 == 0 {
                    info!(sent, elapsed_ms = started.elapsed().as_millis() as u64,
                        "tx progress");
                }
            }
            r = events => {
                r.context("tx stream errored mid-send")?;
                bail!("tx stream ended mid-send (sent={sent}/{frames})");
            }
        }
    }

    // Voice-stop = same packet shape, empty Opus payload.
    info!(sent, "sending voice-stop");
    let stop = OutAudio::new(&AudioData::C2S {
        id: 0,
        codec,
        data: &[],
    });
    con.send_audio(stop).context("send_audio voice-stop")?;

    // Give the resend loop a moment to flush the tail packets before we tear
    // the connection down.
    {
        let events = con.events();
        tokio::pin!(events);
        let drain_for = tokio::time::sleep(Duration::from_millis(500));
        tokio::pin!(drain_for);
        loop {
            tokio::select! {
                biased;
                _ = &mut drain_for => break,
                ev = events.next() => match ev {
                    Some(Ok(_)) => continue,
                    Some(Err(err)) => {
                        warn!(?err, "tx stream error during drain");
                        break;
                    }
                    None => break,
                }
            }
        }
    }

    info!(sent, "tx complete; disconnecting");
    if let Err(err) = con.disconnect(
        DisconnectOptions::new()
            .reason(Reason::Clientdisconnect)
            .message("audio_e2e tx done"),
    ) {
        warn!(?err, "tx disconnect call failed");
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        let drain = con.events();
        tokio::pin!(drain);
        while drain.next().await.is_some() {}
    })
    .await;
    Ok(sent)
}

fn synth_sine(freq_hz: f32, samples: usize) -> Vec<f32> {
    let two_pi = std::f32::consts::PI * 2.0;
    let dt = 1.0 / 48_000.0;
    (0..samples)
        .map(|n| (two_pi * freq_hz * (n as f32) * dt).sin() * 0.25)
        .collect()
}

fn env_flag(name: &str) -> bool {
    matches!(
        env::var(name).as_deref(),
        Ok("1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON")
    )
}

fn env_u32(name: &str, default: u32) -> u32 {
    env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_f64(name: &str, default: f64) -> f64 {
    env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn wedged_fixture_bail(what: &str, addr: &str) -> Result<()> {
    bail!(
        "{what} on {addr} within {}s. \
         Symptom of a wedged fixture (PURA-105 — passt port-forward stalls \
         after 5 sequential WebQuery requests). Bring the fixture up with \
         `podman-compose --profile ts6-fixture up -d ts6-fixture` (which \
         enforces `network_mode: host`) and retry. See docs/ts6-fixture.md.",
        HANDSHAKE_TIMEOUT.as_secs(),
    )
}
