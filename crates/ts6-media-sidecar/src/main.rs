//! `ts6-media-sidecar` binary entry point. PURA-139 (WS-1) — scaffold
//! only: boots the QUIC/WebTransport listener (ALPN-pinned to
//! `moq-lite-04`) + the control-plane HTTP server. No real media yet.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

use ts6_media_sidecar::{Sidecar, SidecarConfig, TransportConfig};

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

    tokio::select! {
        res = sidecar.join() => res,
        _ = tokio::signal::ctrl_c() => {
            info!("ctrl_c received, shutting down");
            Ok(())
        }
    }
}
