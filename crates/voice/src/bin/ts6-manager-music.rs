//! Contabo Music+Voice unit — decode → Opus → TS6 wire send.
//!
//! Panel/API/Surreal stay on fullstack. This process is the only send
//! loop. Chat parsing lives here (TS client), not a second parser on
//! fullstack. No Surreal. Wire marks (`first_frame_on_wire`,
//! `music_bot_latency`) stay in-process.
//!
//! Intra-container affinity (option 1): `TS6_BOT_SEND_CPUSET` /
//! `TS6_BOT_CPUSET` pins `voice-rt` send threads only. ffmpeg / yt-dlp
//! stay unpinned unless `TS6_BOT_DECODE_CPUSET` is set later.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Parser, Debug)]
#[command(
    name = "ts6-manager-music",
    about = "TS6 Manager Music+Voice unit (Contabo bot container)"
)]
struct Args {
    /// Loopback control plane. hostNetwork: containerPort IS host port.
    #[arg(long, default_value = "0.0.0.0:3002")]
    listen: SocketAddr,

    /// Probe `/health` and exit (0 = healthy). Kube exec HealthCmd /
    /// OCI HEALTHCHECK — this image does not ship curl.
    #[arg(
        long = "healthcheck-url",
        value_name = "URL",
        num_args = 0..=1,
        default_missing_value = music_bot::healthcheck::DEFAULT_URL
    )]
    healthcheck_url: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    if let Some(url) = args.healthcheck_url {
        return music_bot::healthcheck::probe(&url).await;
    }

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info,music_bot=info,music_bot_audio=info,music_bot_latency=info")
    });
    tracing_subscriber::registry()
        .with(env_filter)
        .with(music_bot::bug_report::layer())
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .flatten_event(true)
                .with_target(true)
                .with_current_span(false)
                .with_span_list(false),
        )
        .init();

    music_bot_audio::cpuset::validate_send_vs_decode()
        .map_err(|e| anyhow::anyhow!(e))
        .context("send vs decode cpuset")?;

    // PURA-359 — warm yt-dlp here, not on fullstack, so import cost
    // never shares the fullstack runqueue with Axum/Surreal/Scuffed.
    music_bot::warm_resolver();

    let music_dir =
        std::env::var("MUSIC_DIR").unwrap_or_else(|_| "/var/lib/ts6-manager/music".into());
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "/var/lib/ts6-manager/data".into());
    info!(
        listen = %args.listen,
        music_dir,
        data_dir,
        send_cpuset = %std::env::var("TS6_BOT_SEND_CPUSET")
            .or_else(|_| std::env::var("TS6_BOT_CPUSET"))
            .unwrap_or_default(),
        decode_cpuset = %std::env::var("TS6_BOT_DECODE_CPUSET").unwrap_or_default(),
        "ts6-manager-music starting (owns the only Voice send loop; no Surreal)"
    );
    let _ = PathBuf::from(music_dir);
    let _ = PathBuf::from(data_dir);

    let state = music_bot::runtime_http::RuntimeState::new();
    if let Ok(cookie) = std::env::var("YT_COOKIE_FILE")
        && !cookie.is_empty()
    {
        *state.yt_cookie.write().unwrap_or_else(|e| e.into_inner()) = Some(PathBuf::from(cookie));
    }
    if let Ok(key) = std::env::var("YOUTUBE_API_KEY")
        && !key.is_empty()
    {
        *state.yt_api_key.write().unwrap_or_else(|e| e.into_inner()) = Some(key);
    }

    let app = music_bot::runtime_http::router(state);
    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("bind {}", args.listen))?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("serve music control plane")
}
