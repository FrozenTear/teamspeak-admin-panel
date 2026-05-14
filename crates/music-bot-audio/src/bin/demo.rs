//! Frame-stats demo binary — exercises the music-bot audio pipeline without
//! the bot lifecycle (WS-1) being wired up. Useful for:
//!
//! - eyeballing 20 ms pacing against the wall clock
//! - dumping ICY `NowPlaying` events from a real radio URL
//! - smoke-testing yt-dlp / ffmpeg availability on the operator host
//!
//! Recipe + sample output live in `docs/voice/audio-pipeline.md`.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use music_bot_audio::AudioPipeline;
use music_bot_audio::source::AudioSourceSpec;
use music_bot_audio::types::{OpusApplication, PipelineConfig, PipelineEvent};
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};

#[derive(Parser, Debug)]
#[command(
    name = "music-bot-audio-demo",
    about = "Pipeline demo — prints frame stats and ICY events (PURA-119 WS-2)"
)]
struct Cli {
    #[command(subcommand)]
    source: SourceCmd,

    /// Channels: 1 = mono (TS6 voice default), 2 = stereo.
    #[arg(long, default_value_t = 1)]
    channels: u8,

    /// Opus encoder bitrate in bits/sec. Default 64 kbps.
    #[arg(long, default_value_t = 64_000)]
    bitrate: i32,

    /// Stop after N frames. 0 = run until EOF / Ctrl-C.
    #[arg(long, default_value_t = 0)]
    max_frames: u64,

    /// If set, the demo waits until each frame's `scheduled_at` before
    /// recording its arrival time — closer to what WS-1 will see in practice.
    /// Off by default so encode-only throughput is measured.
    #[arg(long)]
    pace: bool,
}

#[derive(Subcommand, Debug)]
enum SourceCmd {
    /// Synthetic sine-wave tone.
    Tone {
        #[arg(long, default_value_t = 440.0)]
        hz: f32,
        #[arg(long, default_value_t = 0.5)]
        amplitude: f32,
        /// Stop after this many milliseconds. 0 = forever.
        #[arg(long, default_value_t = 0)]
        duration_ms: u64,
    },
    /// Decode any ffmpeg-readable input (file path, http URL, etc.).
    Ffmpeg {
        #[arg(long)]
        input: String,
    },
    /// yt-dlp → ffmpeg pipeline.
    YtDlp {
        #[arg(long)]
        url: String,
    },
    /// ICY radio source (Shoutcast / Icecast).
    Icy {
        #[arg(long)]
        url: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,music_bot_audio=debug".into()),
        )
        .init();
    let cli = Cli::parse();

    let cfg = PipelineConfig {
        channels: cli.channels,
        bitrate_bps: Some(cli.bitrate),
        application: OpusApplication::Audio,
        frame_buffer: 16,
        event_buffer: 32,
    };

    let spec = match cli.source {
        SourceCmd::Tone {
            hz,
            amplitude,
            duration_ms,
        } => AudioSourceSpec::SyntheticTone {
            hz,
            amplitude,
            duration_ms: if duration_ms == 0 {
                None
            } else {
                Some(duration_ms)
            },
        },
        SourceCmd::Ffmpeg { input } => AudioSourceSpec::Ffmpeg { input },
        SourceCmd::YtDlp { url } => AudioSourceSpec::YtDlp { url },
        SourceCmd::Icy { url } => AudioSourceSpec::IcyRadio { url },
    };

    info!(
        ?spec,
        channels = cli.channels,
        bitrate = cli.bitrate,
        pace = cli.pace,
        "starting pipeline"
    );
    let mut pipeline = AudioPipeline::spawn(spec, cfg)
        .await
        .context("AudioPipeline::spawn")?;
    let mut frames = pipeline.take_frames();
    let mut events = pipeline.events();

    let mut events_task = tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(PipelineEvent::NowPlaying { title, source }) => {
                    info!(%title, %source, "NowPlaying");
                }
                Ok(PipelineEvent::EndOfStream) => {
                    info!("EndOfStream event");
                    break;
                }
                Ok(PipelineEvent::Warning(msg)) => warn!(%msg, "pipeline warning"),
                Err(RecvError::Lagged(n)) => warn!(missed = n, "event subscriber lagged"),
                Err(RecvError::Closed) => break,
            }
        }
    });

    let started = Instant::now();
    let mut received = 0u64;
    let mut total_bytes = 0u64;
    let mut max_jitter_ms = 0i64;
    while let Some(frame) = frames.recv().await {
        received += 1;
        total_bytes += frame.bytes.len() as u64;
        if cli.pace {
            // Wait until the pacer says it's time, then measure.
            let now = Instant::now();
            if frame.scheduled_at > now {
                tokio::time::sleep_until(frame.scheduled_at.into()).await;
            }
            let actual = Instant::now();
            let target = frame.scheduled_at;
            let drift_ms = if actual >= target {
                actual.duration_since(target).as_millis() as i64
            } else {
                -(target.duration_since(actual).as_millis() as i64)
            };
            if drift_ms.abs() > max_jitter_ms.abs() {
                max_jitter_ms = drift_ms;
            }
        }
        if received.is_multiple_of(50) {
            let elapsed = started.elapsed();
            info!(
                frame = received,
                bytes = total_bytes,
                avg_bytes_per_frame = total_bytes / received,
                elapsed_ms = elapsed.as_millis() as u64,
                max_jitter_ms,
                "stats"
            );
        }
        if cli.max_frames > 0 && received >= cli.max_frames {
            break;
        }
    }

    let elapsed = started.elapsed();
    info!(
        frames = received,
        bytes = total_bytes,
        elapsed_ms = elapsed.as_millis() as u64,
        max_jitter_ms,
        "exit"
    );

    pipeline.shutdown().await;
    events_task.abort();
    let _ = (&mut events_task).await;

    // Sanity hint: if pacing was on, jitter should sit comfortably under
    // 20 ms per frame.
    if cli.pace && elapsed > Duration::from_millis(100) {
        let expected_ms = received as i64 * 20;
        let actual_ms = elapsed.as_millis() as i64;
        info!(
            expected_ms,
            actual_ms, "wall-clock vs paced frame count: |delta| should be tiny"
        );
    }

    Ok(())
}
