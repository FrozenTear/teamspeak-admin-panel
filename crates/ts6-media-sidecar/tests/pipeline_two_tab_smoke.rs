//! WS-2 acceptance: FFmpeg → MoQ → two subscribers, in-process.
//!
//! Boots the sidecar lib on ephemeral ports, starts a [`Pipeline`] that
//! drives FFmpeg from a synthetic `lavfi` source, then connects two
//! `moq-native` client sessions to the sidecar and asserts each one
//! receives at least one video frame AND at least one audio frame on
//! the `"video"` / `"audio"` tracks the WS-0 reference player speaks.
//!
//! Why `lavfi` rather than the promoted `tests/fixtures/sample.mp4`:
//! lavfi keeps the test self-contained (no fixture pre-build step in
//! CI). Operator-side smoke against `sample.mp4` is the
//! `tests/fixtures/build.sh` recipe, exercised manually + by the
//! `moq-spike/player/` reference player.
//!
//! Hides behind `#[cfg_attr(not(feature = "ffmpeg-smoke"), ignore)]` so
//! `cargo test --no-run` and `cargo build` stay ffmpeg-free; the smoke
//! itself runs under `cargo test -p ts6-media-sidecar -- --include-ignored`
//! or `cargo test --features ffmpeg-smoke`.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use moq_lite::{Origin, OriginConsumer, Track};
use moq_native::ClientConfig;
use tokio::time::timeout;
use ts6_media_sidecar::{
    GaiResolver, Pipeline, PipelineConfig, Sidecar, SidecarConfig, SourceInput, TransportConfig,
};
use url::Url;

const BROADCAST_NAME: &str = "smoke-source";
const TRACK_VIDEO: &str = "video";
const TRACK_AUDIO: &str = "audio";

/// Soft cap for the whole smoke. FFmpeg cold-start on a CI VM is the
/// slowest step; 60s leaves room for libvpx encoder init + the first
/// keyframe + 50 Opus packets before the assertion deadline.
const SMOKE_DEADLINE: Duration = Duration::from_secs(60);

/// How long to wait for the broadcast to be announced to the client
/// origin. FFmpeg first-frame latency dominates this.
const ANNOUNCE_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test]
#[cfg_attr(not(feature = "ffmpeg-smoke"), ignore)]
async fn pipeline_two_tab_smoke() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("warn,ts6_media_sidecar=debug,moq_native=info,moq_lite=info")
        .with_test_writer()
        .try_init();

    timeout(SMOKE_DEADLINE, run_smoke())
        .await
        .expect("WS-2 two-tab smoke deadline exceeded")
        .expect("WS-2 two-tab smoke");
}

async fn run_smoke() -> Result<()> {
    // 1. Boot sidecar on ephemeral ports w/ self-signed cert.
    let sidecar = Sidecar::start(SidecarConfig {
        transport: TransportConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            tls_cert: vec![],
            tls_key: vec![],
            tls_generate: vec!["localhost".to_string()],
        },
        http_listen: "127.0.0.1:0".parse().unwrap(),
        resolver: Arc::new(GaiResolver::new()),
        ffmpeg_path: PathBuf::from("ffmpeg"),
    })
    .await
    .context("start sidecar")?;

    // Use 127.0.0.1 explicitly: the sidecar binds to it, and macOS /
    // Linux may resolve `localhost` to ::1 first which moq-native won't
    // dual-stack onto our v4-only listener.
    let transport_port = sidecar.transport_addr.port();
    let url = Url::parse(&format!("https://127.0.0.1:{transport_port}/anon"))
        .context("build subscriber URL")?;

    // 2. Start the FFmpeg pipeline with a synthetic lavfi source. The
    // video subprocess and audio subprocess each get their own lavfi
    // spec (one filter chain per subprocess). 10s duration gives the
    // mux loop ~150 video frames + ~500 Opus packets to publish before
    // ffmpeg exits and the supervisor restarts it.
    let pipeline = Pipeline::start(
        PipelineConfig::new(
            BROADCAST_NAME,
            SourceInput::Lavfi {
                video: "testsrc2=size=320x240:rate=15:duration=10".into(),
                audio: "sine=frequency=440:duration=10:sample_rate=48000".into(),
            },
        ),
        Arc::clone(&sidecar.origin),
    )
    .await
    .context("start pipeline")?;
    // Pipeline keeps running even after the lavfi clip ends — the
    // supervisor restarts FFmpeg with backoff, so subscribers see
    // continuous output. We stop it explicitly at the end.

    // 3. Spawn two independent subscribers in parallel.
    let sub_a = tokio::spawn(run_subscriber("subscriber-A", url.clone()));
    let sub_b = tokio::spawn(run_subscriber("subscriber-B", url));

    let (res_a, res_b) = tokio::try_join!(flatten(sub_a), flatten(sub_b))
        .context("one or both subscribers failed")?;

    assert!(
        res_a.video_frames >= 1 && res_a.audio_frames >= 1,
        "subscriber A: expected ≥1 video AND ≥1 audio frame, got {:?}",
        res_a
    );
    assert!(
        res_b.video_frames >= 1 && res_b.audio_frames >= 1,
        "subscriber B: expected ≥1 video AND ≥1 audio frame, got {:?}",
        res_b
    );

    pipeline.stop().await;
    sidecar.shutdown();
    Ok(())
}

async fn flatten<T>(
    handle: tokio::task::JoinHandle<Result<T>>,
) -> Result<T> {
    match handle.await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(err)) => Err(err),
        Err(join) => Err(anyhow!("subscriber task panicked: {join}")),
    }
}

#[derive(Debug)]
struct SubscriberResult {
    video_frames: u64,
    audio_frames: u64,
}

async fn run_subscriber(label: &'static str, url: Url) -> Result<SubscriberResult> {
    tracing::info!(label, %url, "subscriber connecting");

    // Self-signed cert: disable verification for the smoke. Production
    // uses serverCertificateHashes (browser) or proper PKI (Rust).
    let mut client_cfg = ClientConfig::default();
    client_cfg.tls.disable_verify = Some(true);
    // Pin ALPN to the same draft the server advertises. moq-native's
    // default = "all"; pinning makes the smoke deterministic.
    client_cfg.version = vec![
        moq_lite::Version::from_str("moq-lite-04")
            .map_err(|e| anyhow!("parse moq version: {e}"))?,
    ];
    let client = client_cfg.init().context("init moq-native client")?;

    // Client receives broadcasts into this OriginProducer.
    let producer = Origin::random().produce();
    let consumer: OriginConsumer = producer.consume();

    let session = client
        .with_consume(producer)
        .connect(url)
        .await
        .context("client connect")?;
    tracing::info!(label, version = %session.version(), "subscriber connected");

    let broadcast = timeout(ANNOUNCE_TIMEOUT, async {
        loop {
            if let Some(b) = consumer.get_broadcast(BROADCAST_NAME) {
                return b;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .with_context(|| format!("{label}: broadcast '{BROADCAST_NAME}' never announced"))?;
    tracing::info!(label, "subscriber saw broadcast announcement");

    let mut video_track = broadcast
        .subscribe_track(&Track::new(TRACK_VIDEO))
        .context("subscribe video track")?;
    let mut audio_track = broadcast
        .subscribe_track(&Track::new(TRACK_AUDIO))
        .context("subscribe audio track")?;

    // Read at least one frame from each track. Use parallel polling so
    // a stalled track doesn't deadlock the other.
    let video = tokio::spawn(async move {
        let group = video_track
            .next_group()
            .await
            .context("video next_group")?
            .ok_or_else(|| anyhow!("video track closed before first group"))?;
        let mut group = group;
        let frame = group
            .read_frame()
            .await
            .context("video read_frame")?
            .ok_or_else(|| anyhow!("video group had no frame"))?;
        anyhow::Ok((frame.len() as u64, 1u64))
    });

    let audio = tokio::spawn(async move {
        let group = audio_track
            .next_group()
            .await
            .context("audio next_group")?
            .ok_or_else(|| anyhow!("audio track closed before first group"))?;
        let mut group = group;
        let frame = group
            .read_frame()
            .await
            .context("audio read_frame")?
            .ok_or_else(|| anyhow!("audio group had no frame"))?;
        anyhow::Ok((frame.len() as u64, 1u64))
    });

    let (vres, ares) = tokio::join!(video, audio);
    let (vbytes, vframes) = vres.expect("video task")?;
    let (abytes, aframes) = ares.expect("audio task")?;

    tracing::info!(
        label,
        video_first_frame_bytes = vbytes,
        audio_first_frame_bytes = abytes,
        "subscriber drained first frames"
    );

    Ok(SubscriberResult {
        video_frames: vframes,
        audio_frames: aframes,
    })
}
