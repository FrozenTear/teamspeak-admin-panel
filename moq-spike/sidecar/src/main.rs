//! moq-spike-sidecar — WS-0 publisher stub.
//!
//! See ../README.md and ../../docs/adr/0007-moq-flavor-and-draft-pin.md.
//!
//! Heartbeat 1 of [PURA-138](/PURA/issues/PURA-138) lands the
//! scaffolding and validates the crate pin compiles. The IVF/Ogg
//! reader + frame-pacing loop lands in heartbeat 2.

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use tracing::info;
use url::Url;

#[derive(Parser, Debug)]
#[command(name = "moq-spike-sidecar", version, about = "WS-0 MoQ publisher", long_about = None)]
struct Args {
    /// Relay URL, e.g. https://localhost:4443
    #[arg(long)]
    relay: Url,

    /// Broadcast namespace to announce, e.g. pura-spike/0
    #[arg(long, default_value = "pura-spike/0")]
    namespace: String,

    /// Path to VP8-in-IVF video fixture
    #[arg(long)]
    video: PathBuf,

    /// Path to Opus-in-Ogg audio fixture
    #[arg(long)]
    audio: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,moq_lite=debug")),
        )
        .init();

    let args = Args::parse();
    info!(
        relay = %args.relay,
        namespace = %args.namespace,
        video = %args.video.display(),
        audio = %args.audio.display(),
        "moq-spike-sidecar starting"
    );

    // Heartbeat-2 work: connect via moq_native::Client, announce a
    // BroadcastProducer on `args.namespace`, open `video` + `audio`
    // TrackProducers, then pace IVF + Ogg frames as `GroupProducer`s.
    // The exact API shape is documented in docs/adr/0007 §Decision.
    anyhow::bail!(
        "sidecar publish loop is not yet implemented — heartbeat 2 of PURA-138 \
         lands the IVF/Ogg reader and moq-lite publish pipeline"
    )
}
