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

use crate::livekit::{LiveKitConfig, StubLiveKitBridge};
use crate::ts6::{Ts6Config, Ts6Connection};

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

    // 3. Stub LiveKit bridge — slice c plugs in the real Rust SDK.
    let mut bridge = StubLiveKitBridge::connect(&livekit_config, &token).await?;
    info!(state = ?bridge.state(), "LiveKit bridge stub up");

    // 4. Idle until duration. Slice c-e wires the actual Opus forwarding
    //    into this loop. For the scaffold we drain TS6 events to keep
    //    the connection healthy and emit a heartbeat log every 10 s.
    let deadline = tokio::time::sleep(Duration::from_secs(cli.duration_secs));
    tokio::pin!(deadline);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(10));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut tick = 0u64;
    let started = Instant::now();
    let mut events_seen = 0u64;

    'outer: loop {
        // Borrow-checker dance from `ts6-voice-prototype`: the events
        // stream borrows the inner `Connection` for as long as it
        // lives, which would conflict with future slice-c calls to
        // `con.send_audio(...)` inside other arms. Create the events
        // future inline so `tokio::select!` drops the non-selected
        // arms' futures before running the chosen arm's body.
        tokio::select! {
            biased;
            _ = &mut deadline => {
                info!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    events_seen,
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
                    bridge_state = ?bridge.state(),
                    "heartbeat"
                );
            }
            ev = async { ts6.raw().events().next().await } => match ev {
                Some(Ok(_item)) => {
                    events_seen += 1;
                    // Slice c: forward AudioData::S2C / S2CWhisper into
                    // `bridge.publish_opus_frame(...)` here.
                }
                Some(Err(err)) => {
                    error!(?err, "TS6 stream error during scaffold idle");
                    break 'outer;
                }
                None => {
                    warn!("TS6 stream ended during scaffold idle");
                    break 'outer;
                }
            }
        }
    }

    bridge.disconnect().await?;
    ts6.disconnect("ts6-voice-translator shutdown").await;
    info!(events_seen, "ts6-voice-translator exited cleanly");
    Ok(())
}
