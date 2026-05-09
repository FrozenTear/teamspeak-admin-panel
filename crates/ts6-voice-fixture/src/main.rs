// PURA-106 — Headless TS6 voice-client fixture for V4/V7 visual e2e.
//
// Connect-only client: generates (or loads) an identity, connects to a
// TS6 server's voice port, and stays connected until SIGINT/SIGTERM. No
// audio capture/playback. Spawn N instances in parallel by giving each a
// distinct `--identity-dir` and `--name`.
//
// Shared bring-up helpers live in `src/lib.rs` so the PURA-110 audio-E2E
// integration test can drive the same handshake / identity bootstrap.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use tokio::signal;
use tracing::{debug, error, info, warn};
use ts6_voice_fixture::{load_or_create_identity, wait_for_connected};
use tsclientlib::{Connection, DisconnectOptions, Reason, StreamItem};

#[derive(Debug, Parser)]
#[command(
    name = "ts6-voice-fixture",
    about = "Headless connect-only TS6 voice client for QA / CI",
    long_about = "Connects to a TS6 server's voice channel using the TS3 protocol.\n\
        Stays online until SIGINT/SIGTERM. To run two instances, give each a\n\
        different --identity-dir + --name."
)]
struct Cli {
    /// Server host:port — defaults to 127.0.0.1:9987 (TS6 voice).
    #[arg(long, default_value = "127.0.0.1:9987")]
    server: String,

    /// Display name in-channel.
    #[arg(long, default_value = "qa-fixture")]
    name: String,

    /// Per-instance state directory. Holds the cached identity.
    /// Two simultaneous instances MUST use different dirs.
    #[arg(long, default_value = "./.ts6-voice-fixture")]
    identity_dir: PathBuf,

    /// How long to wait for the connection handshake before giving up.
    #[arg(long, default_value_t = 20)]
    connect_timeout_secs: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,ts6_voice_fixture=debug".into()),
        )
        .init();

    let cli = Cli::parse();
    info!(
        server = %cli.server,
        name = %cli.name,
        identity_dir = %cli.identity_dir.display(),
        "ts6-voice-fixture starting",
    );

    tokio::fs::create_dir_all(&cli.identity_dir)
        .await
        .with_context(|| format!("create identity dir {}", cli.identity_dir.display()))?;

    let identity_path = cli.identity_dir.join("identity.json");
    let identity = load_or_create_identity(&identity_path).await?;
    info!(uid = %identity.key().to_pub().get_uid(), "identity ready");

    let mut con = Connection::build(cli.server.as_str())
        .name(cli.name.clone())
        .identity(identity)
        .log_commands(false)
        .log_packets(false)
        .log_udp_packets(false)
        .connect()
        .context("ConnectOptions::connect()")?;

    info!("connection object created — driving handshake");

    if !wait_for_connected(&mut con, Duration::from_secs(cli.connect_timeout_secs)).await? {
        anyhow::bail!(
            "connection did not reach Connected state within {}s — \
             likely TS6 handshake delta. See PURA-106 spike memo.",
            cli.connect_timeout_secs
        );
    }

    info!("connected — idle until Ctrl-C / SIGTERM");

    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())?;
    let mut sigint = signal::unix::signal(signal::unix::SignalKind::interrupt())?;

    // Pump events while idle so the underlying ping loop keeps the session
    // alive. tsclientlib's contract: "the connection will not do anything
    // unless the event stream is polled". We pin the stream once and reuse
    // it; the &mut con borrow is released when the inner block ends.
    {
        let events = con.events();
        tokio::pin!(events);
        loop {
            tokio::select! {
                _ = sigint.recv() => { info!("SIGINT received"); break; }
                _ = sigterm.recv() => { info!("SIGTERM received"); break; }
                item = events.next() => match item {
                    Some(Ok(StreamItem::DisconnectedTemporarily(reason))) => {
                        warn!(?reason, "temporary disconnect; tsclientlib will retry");
                    }
                    Some(Ok(other)) => debug!(?other, "stream item while idle"),
                    Some(Err(err)) => {
                        error!(?err, "stream error");
                        break;
                    }
                    None => {
                        warn!("event stream ended");
                        break;
                    }
                }
            }
        }
    }

    info!("disconnecting");
    if let Err(err) = con.disconnect(
        DisconnectOptions::new()
            .reason(Reason::Clientdisconnect)
            .message("ts6-voice-fixture shutdown"),
    ) {
        warn!(?err, "disconnect call failed");
    }

    // Allow the disconnect frame to flush before tearing the runtime down.
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        let drain = con.events();
        tokio::pin!(drain);
        while drain.next().await.is_some() {}
    })
    .await;

    info!("ts6-voice-fixture exited cleanly");
    Ok(())
}
