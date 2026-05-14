//! `ts6-media-sidecar` binary entry point. PURA-139 (WS-1) — scaffold
//! only: boots the QUIC/WebTransport listener (ALPN-pinned to
//! `moq-lite-04`) + the control-plane HTTP server. No real media yet.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

use ts6_media_sidecar::{
    Pipeline, PipelineConfig, Sidecar, SidecarConfig, SourceInput, TransportConfig,
};

#[derive(Parser, Debug)]
#[command(name = "ts6-media-sidecar", about = "Phase-5 MoQ + WebTransport sidecar")]
struct Args {
    /// UDP socket for QUIC / WebTransport (e.g. `[::]:4443`).
    #[arg(long, default_value = "[::]:4443")]
    listen: SocketAddr,

    /// TCP socket for the control-plane HTTP server (e.g. `127.0.0.1:7080`).
    #[arg(long = "http-listen", default_value = "127.0.0.1:7080")]
    http_listen: SocketAddr,

    /// PEM cert chain (repeatable). Required unless `--tls-generate` is set.
    #[arg(long = "cert", conflicts_with = "tls_generate")]
    cert: Vec<PathBuf>,

    /// PEM private key matching `--cert` (repeatable). Required if `--cert` is set.
    #[arg(long = "key", conflicts_with = "tls_generate")]
    key: Vec<PathBuf>,

    /// Generate an in-memory self-signed certificate for these hostnames.
    /// Dev / smoke-test only — production should ship `--cert`/`--key`.
    #[arg(long = "tls-generate", value_delimiter = ',')]
    tls_generate: Vec<String>,

    /// Optional: start a media pipeline at boot. Becomes the broadcast
    /// name browsers subscribe to. Mutating REST control is WS-3.
    #[arg(long = "source-name", requires = "source")]
    source_name: Option<String>,

    /// Optional: anything FFmpeg can read from. Mutually exclusive with
    /// `--source-lavfi-video`/`--source-lavfi-audio`.
    #[arg(long = "source", conflicts_with_all = ["source_lavfi_video", "source_lavfi_audio"], requires = "source_name")]
    source: Option<String>,

    /// Optional: synthetic FFmpeg video source spec, e.g.
    /// `testsrc2=size=320x240:rate=15`. Pair with `--source-lavfi-audio`.
    #[arg(long = "source-lavfi-video", requires_all = ["source_name", "source_lavfi_audio"], conflicts_with = "source")]
    source_lavfi_video: Option<String>,

    /// Optional: synthetic FFmpeg audio source spec, e.g.
    /// `sine=frequency=440:sample_rate=48000`. Pair with `--source-lavfi-video`.
    #[arg(long = "source-lavfi-audio", requires_all = ["source_name", "source_lavfi_video"], conflicts_with = "source")]
    source_lavfi_audio: Option<String>,

    /// Path to the ffmpeg binary. Defaults to `ffmpeg` on PATH.
    #[arg(long = "ffmpeg-path", default_value = "ffmpeg")]
    ffmpeg_path: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new("info,ts6_media_sidecar=debug,moq_native=info,moq_lite=info")
        }))
        .init();

    let args = Args::parse();

    let config = SidecarConfig {
        transport: TransportConfig {
            bind: args.listen,
            tls_cert: args.cert,
            tls_key: args.key,
            tls_generate: args.tls_generate,
        },
        http_listen: args.http_listen,
    };

    let sidecar = Sidecar::start(config).await.context("start sidecar")?;
    info!(
        transport = %sidecar.transport_addr,
        http = %sidecar.http_addr,
        fingerprint = %sidecar.fingerprint,
        "ts6-media-sidecar up"
    );

    let pipeline = match (
        args.source_name,
        args.source,
        args.source_lavfi_video,
        args.source_lavfi_audio,
    ) {
        (Some(name), Some(url), _, _) => Some(
            Pipeline::start(
                PipelineConfig::new(name, SourceInput::Url(url))
                    .with_ffmpeg_path(args.ffmpeg_path.clone()),
                sidecar.origin.clone(),
            )
            .await
            .context("start pipeline")?,
        ),
        (Some(name), None, Some(video), Some(audio)) => Some(
            Pipeline::start(
                PipelineConfig::new(name, SourceInput::Lavfi { video, audio })
                    .with_ffmpeg_path(args.ffmpeg_path.clone()),
                sidecar.origin.clone(),
            )
            .await
            .context("start pipeline")?,
        ),
        _ => None,
    };

    tokio::select! {
        res = sidecar.join() => {
            if let Some(p) = pipeline { p.stop().await; }
            res
        }
        _ = tokio::signal::ctrl_c() => {
            info!("ctrl_c received, shutting down");
            if let Some(p) = pipeline { p.stop().await; }
            Ok(())
        }
    }
}
