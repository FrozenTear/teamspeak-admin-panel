// PURA-108 WS-7 / PURA-114 — `ts6-voice-translator` daemon scaffold (slice b).
//
// Single binary that joins a self-hosted TS6 voice room as a synthetic
// `tsclientlib` client and bridges its audio to a self-hosted LiveKit
// room. Slice b establishes both legs of the bridge: TS6 handshake
// completes, the LiveKit access token mints + validates against the
// configured key/secret, and the daemon stays alive draining TS6 events
// until the requested duration elapses, then disconnects cleanly.
//
// Subsequent slices fill in the audio forwarding:
//   - WS-7c — TS6 → LiveKit half-duplex (publish inbound TS6 Opus into
//     LiveKit as a publisher track; browser tab can hear native clients).
//   - WS-7d — LiveKit → TS6 reverse path.
//   - WS-7e — Browser demo + acceptance recipe (≥30 s bidirectional voice).
//
// The TS6 handshake plumbing here is lifted from
// `crates/ts6-voice-prototype` (PURA-112). The LiveKit access-token
// minter is pure JWT against the public LiveKit auth format; the heavy
// `livekit` Rust SDK (which depends on a native libwebrtc build) lands
// with slice c.

mod livekit;
mod ts6;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use tokio::time::Instant;
use tracing::{error, info, warn};

use crate::livekit::{LiveKitBridge, LiveKitConfig};
use crate::ts6::{Ts6Config, Ts6Connection};
use tsclientlib::StreamItem;
use tsproto_packets::packets::{AudioData, CodecType, OutAudio};

#[derive(Parser, Debug)]
#[command(
    name = "ts6-voice-translator",
    about = "Bridges a TS6 voice room to a LiveKit room (PURA-108 WS-7 slice b scaffold)"
)]
struct Cli {
    /// TS6 voice server, host:port. Default = local podman-compose ts6-fixture.
    #[arg(long, default_value = "127.0.0.1:9987")]
    ts6_server: String,

    /// In-channel display name for the synthetic translator client.
    #[arg(long, default_value = "ts6-voice-translator")]
    ts6_name: String,

    /// Per-instance directory for the cached TS6 identity.
    #[arg(long)]
    identity_dir: PathBuf,

    /// LiveKit signaling URL (typically ws://host:7880 or wss://...).
    #[arg(long, default_value = "ws://127.0.0.1:7880")]
    livekit_url: String,

    /// LiveKit room name to join. Slice c populates this with the TS6
    /// channel id once the bridge is end-to-end.
    #[arg(long, default_value = "ts6-bridge")]
    livekit_room: String,

    /// LiveKit API key. Defaults to the dev key in deploy/voice/livekit.yaml
    /// so the binary runs against the unmodified `voice-translator` profile
    /// out of the box. Production deployments override via env.
    #[arg(long, env = "LIVEKIT_API_KEY", default_value = "devkey")]
    livekit_api_key: String,

    /// LiveKit API secret. Defaults to the dev secret in
    /// deploy/voice/livekit.yaml — the obviously-fake string makes
    /// accidental prod inheritance loud.
    #[arg(
        long,
        env = "LIVEKIT_API_SECRET",
        default_value = "DEV_ONLY_CHANGE_ME_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    )]
    livekit_api_secret: String,

    /// LiveKit participant identity (the translator's "name" inside the
    /// LiveKit room). Distinct from --ts6-name on purpose so logs make
    /// it obvious which side of the bridge a message came from.
    #[arg(long, default_value = "ts6-bridge-translator")]
    livekit_identity: String,

    /// Total run duration in seconds. Slice b idles until this expires;
    /// slice c-e drive a real session.
    #[arg(long, default_value_t = 60)]
    duration_secs: u64,

    /// TS6 handshake timeout.
    #[arg(long, default_value_t = 30)]
    connect_timeout_secs: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,ts6_voice_translator=debug".into()),
        )
        .init();
    let cli = Cli::parse();

    info!(
        ts6_server = %cli.ts6_server,
        ts6_name = %cli.ts6_name,
        identity_dir = %cli.identity_dir.display(),
        livekit_url = %cli.livekit_url,
        livekit_room = %cli.livekit_room,
        livekit_identity = %cli.livekit_identity,
        duration_secs = cli.duration_secs,
        "ts6-voice-translator starting"
    );

    // 1. Mint the LiveKit access token. Cheap, deterministic, runs first
    //    so config errors fail before we spend a TS6 handshake on them.
    let livekit_config = LiveKitConfig {
        url: cli.livekit_url.clone(),
        room: cli.livekit_room.clone(),
        identity: cli.livekit_identity.clone(),
        api_key: cli.livekit_api_key.clone(),
        api_secret: cli.livekit_api_secret.clone(),
        ttl: Duration::from_secs(cli.duration_secs.saturating_add(60)),
    };
    let token = livekit_config
        .mint_token()
        .context("mint LiveKit access token")?;
    info!(
        token_len = token.len(),
        "LiveKit access token minted (paste into a LiveKit Web SDK demo to verify the round-trip)"
    );

    // 2. Drive the TS6 handshake. Lifted from `ts6-voice-prototype`
    //    (PURA-112). Slice b stops at "BookEvents arrived"; slice c will
    //    keep the connection up and forward inbound Opus.
    let ts6_config = Ts6Config {
        server: cli.ts6_server.clone(),
        name: cli.ts6_name.clone(),
        identity_dir: cli.identity_dir.clone(),
        connect_timeout: Duration::from_secs(cli.connect_timeout_secs),
    };
    let mut ts6 = Ts6Connection::connect(&ts6_config).await?;
    info!("TS6 handshake complete");

    // 3. Real LiveKit bridge — `Room::connect` + `LocalAudioTrack` published
    //    as `TrackSource::Microphone`. Slice d will subscribe to remote
    //    Opus through the same `Room` handle and feed it back into TS6.
    let mut bridge = LiveKitBridge::connect(&livekit_config, &token).await?;
    info!(state = ?bridge.state(), "LiveKit bridge up");

    // 4. Drive the TS6 event stream and forward inbound Opus voice frames
    //    into the LiveKit publisher track. Heartbeat every 10 s carries the
    //    forwarded-frame count so an operator can confirm audio is flowing
    //    without having to grep the WAV outputs.
    let deadline = tokio::time::sleep(Duration::from_secs(cli.duration_secs));
    tokio::pin!(deadline);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(10));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut tick = 0u64;
    let started = Instant::now();
    let mut events_seen = 0u64;
    let mut audio_frames_seen = 0u64;
    let mut audio_frames_published = 0u64;
    let mut reverse_frames_received = 0u64;
    let mut reverse_frames_sent = 0u64;

    'outer: loop {
        // Borrow-checker dance from `ts6-voice-prototype`: the events
        // stream borrows the inner `Connection` for as long as it
        // lives, which would conflict with `con.send_audio(...)` calls
        // inside other arms. Create the events future inline so
        // `tokio::select!` drops the non-selected arms' futures
        // before running the chosen arm's body.
        //
        // No `biased;` here: TS6 events and LiveKit inbound Opus both
        // arrive at ~50 fps. With biased ordering the events arm wins
        // every tie and starves `recv_inbound_opus`, leaving the
        // reverse-path channel full and the SDK's audio queue
        // overflowing. Random selection drains both fairly.
        tokio::select! {
            _ = &mut deadline => {
                info!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    events_seen,
                    audio_frames_seen,
                    audio_frames_published,
                    reverse_frames_received,
                    reverse_frames_sent,
                    "duration reached — shutting down"
                );
                break 'outer;
            }
            _ = heartbeat.tick() => {
                tick += 1;
                info!(
                    tick,
                    elapsed_s = started.elapsed().as_secs(),
                    events_seen,
                    audio_frames_seen,
                    audio_frames_published,
                    reverse_frames_received,
                    reverse_frames_sent,
                    bridge_state = ?bridge.state(),
                    "heartbeat"
                );
            }
            ev = async { ts6.raw().events().next().await } => {
                match ev {
                    Some(Ok(item)) => {
                        events_seen += 1;
                        if let Some((from, opus)) = extract_inbound_opus(&item) {
                            audio_frames_seen += 1;
                            // Decouple lifetime: the `opus` slice borrows the TS6
                            // Connection's read buffer, so we must copy before the
                            // bridge's `.await` resumes the event loop.
                            let owned: Vec<u8> = opus.to_vec();
                            match bridge.publish_opus_frame(from, &owned).await {
                                Ok(()) => audio_frames_published += 1,
                                Err(err) => warn!(?err, from, "publish_opus_frame failed"),
                            }
                        }
                    }
                    Some(Err(err)) => {
                        error!(?err, "TS6 stream error");
                        break 'outer;
                    }
                    None => {
                        warn!("TS6 stream ended");
                        break 'outer;
                    }
                }
            }
            // Reverse path (slice d): browser-side Opus arriving from a
            // remote LiveKit participant, encoded back to 20 ms / 48 kHz /
            // mono Opus by the per-track subscriber. Forward as a
            // synthetic-client send so native TS6 clients hear the browser.
            maybe_opus = bridge.recv_inbound_opus() => {
                match maybe_opus {
                    Some(opus) => {
                        reverse_frames_received += 1;
                        let pkt = OutAudio::new(&AudioData::C2S {
                            id: 0,
                            codec: CodecType::OpusVoice,
                            data: &opus,
                        });
                        match ts6.raw().send_audio(pkt) {
                            Ok(()) => reverse_frames_sent += 1,
                            Err(err) => warn!(?err, "ts6 send_audio for reverse-path frame failed"),
                        }
                    }
                    None => {
                        warn!("LiveKit inbound Opus channel closed — bridge gone");
                        break 'outer;
                    }
                }
            }
        }
    }

    bridge.disconnect().await?;
    ts6.disconnect("ts6-voice-translator shutdown").await;
    info!(
        events_seen,
        audio_frames_seen,
        audio_frames_published,
        reverse_frames_received,
        reverse_frames_sent,
        "ts6-voice-translator exited cleanly"
    );
    Ok(())
}

/// Pull `(speaker_id, opus_bytes)` out of a `StreamItem::Audio(InAudioBuf)`
/// when the inbound packet is an Opus voice / whisper frame. Non-audio
/// stream items and non-Opus codecs return `None`.
fn extract_inbound_opus(item: &StreamItem) -> Option<(u16, &[u8])> {
    let buf = match item {
        StreamItem::Audio(buf) => buf,
        _ => return None,
    };
    let (from, codec, data): (u16, CodecType, &[u8]) = match buf.data().data() {
        AudioData::S2C { from, codec, data, .. } => (*from, *codec, *data),
        AudioData::S2CWhisper { from, codec, data, .. } => (*from, *codec, *data),
        _ => return None,
    };
    if !matches!(codec, CodecType::OpusVoice | CodecType::OpusMusic) {
        return None;
    }
    Some((from, data))
}
