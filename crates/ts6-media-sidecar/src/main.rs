//! `ts6-media-sidecar` binary entry point. PURA-139 (WS-1) — scaffold
//! only: boots the QUIC/WebTransport listener (ALPN-pinned to
//! `moq-lite-04`) + the control-plane HTTP server. No real media yet.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{ArgGroup, Parser};
use tracing::info;
use tracing_subscriber::EnvFilter;

use ts6_media_sidecar::{
    GaiResolver, Pipeline, PipelineConfig, QualityPreset, Sidecar, SidecarConfig, SourceInput,
    TransportConfig,
};

#[derive(Parser, Debug)]
#[command(
    name = "ts6-media-sidecar",
    about = "Phase-5 MoQ + WebTransport sidecar",
    group(
        ArgGroup::new("boot_source")
            .args(["source", "source_lavfi_video"])
            .multiple(false)
    )
)]
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
    ///
    /// Requires either `--source` or the lavfi pair. Requiring only
    /// `--source` made the documented lavfi flags unsatisfiable: they
    /// conflict with `--source` and themselves require `--source-name`.
    #[arg(long = "source-name", requires = "boot_source")]
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

    /// Encode preset for a boot source (`--source` or lavfi). Same strings
    /// as `POST /source`: `480p`, `720p`, `1080p` (case-insensitive).
    /// Omitted means `720p`, matching the control plane.
    ///
    /// A spare-port lavfi WAN smoke should pass `480p`. The default
    /// `720p` encodes 1280×720 at 30 fps.
    #[arg(long, default_value_t = QualityPreset::DEFAULT)]
    preset: QualityPreset,

    /// Path to the ffmpeg binary. Defaults to `ffmpeg` on PATH.
    #[arg(long = "ffmpeg-path", default_value = "ffmpeg")]
    ffmpeg_path: PathBuf,

    /// Probe the control-plane `/health` and exit (0 = healthy).
    ///
    /// Used as the Quadlet `HealthCmd` / OCI `HEALTHCHECK` so the
    /// sidecar runtime image does not need `curl` or `wget`. When the
    /// flag is present without a value, defaults to
    /// `http://127.0.0.1:7080/health` (the `--http-listen` default).
    #[arg(
        long = "healthcheck-url",
        value_name = "URL",
        num_args = 0..=1,
        default_missing_value = ts6_media_sidecar::healthcheck::DEFAULT_URL
    )]
    healthcheck_url: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Healthcheck is a one-shot GET — skip tracing so Quadlet / OCI
    // probe output stays silent on success (curl -fsS equivalent).
    if let Some(url) = args.healthcheck_url {
        return ts6_media_sidecar::healthcheck::probe(&url).await;
    }

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new("info,ts6_media_sidecar=debug,moq_native=info,moq_lite=info")
        }))
        .init();

    // Clone the boot source before `args` is moved into the sidecar.
    // Diagnostics are attached after the sidecar is up.
    let boot_cfg = boot_pipeline_config(&args);
    let config = SidecarConfig {
        transport: TransportConfig {
            bind: args.listen,
            tls_cert: args.cert,
            tls_key: args.key,
            tls_generate: args.tls_generate,
        },
        http_listen: args.http_listen,
        resolver: Arc::new(GaiResolver::new()),
        ffmpeg_path: args.ffmpeg_path.clone(),
    };

    let sidecar = Sidecar::start(config).await.context("start sidecar")?;
    info!(
        transport = %sidecar.transport_addr,
        http = %sidecar.http_addr,
        fingerprint = %sidecar.fingerprint,
        "ts6-media-sidecar up"
    );

    let pipeline = match boot_cfg {
        Some(cfg) => Some(
            Pipeline::start(
                cfg.with_diagnostics(sidecar.diagnostics.clone()),
                sidecar.origin.clone(),
            )
            .await
            .context("start pipeline")?,
        ),
        None => None,
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

/// Pipeline a boot source would start. `None` when the process is
/// control-plane only (no `--source` and no lavfi pair).
///
/// The preset is the CLI `--preset`, default `720p`. Lavfi and file
/// boots share it. Pacing stays on [`SourceInput::from_input`]: lavfi
/// is unpaced, a file path is paced.
fn boot_pipeline_config(args: &Args) -> Option<PipelineConfig> {
    let preset = args.preset;
    let ffmpeg_path = args.ffmpeg_path.clone();
    match (
        args.source_name.clone(),
        args.source.clone(),
        args.source_lavfi_video.clone(),
        args.source_lavfi_audio.clone(),
    ) {
        (Some(name), Some(url), _, _) => Some(
            PipelineConfig::new(name, SourceInput::from_input(url))
                .with_ffmpeg_path(ffmpeg_path)
                .with_preset(preset),
        ),
        (Some(name), None, Some(video), Some(audio)) => Some(
            PipelineConfig::new(name, SourceInput::Lavfi { video, audio })
                .with_ffmpeg_path(ffmpeg_path)
                .with_preset(preset),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, clap::Error> {
        Args::try_parse_from(std::iter::once("ts6-media-sidecar").chain(args.iter().copied()))
    }

    #[test]
    fn lavfi_boot_flags_parse() {
        let args = parse(&[
            "--tls-generate",
            "localhost",
            "--source-name",
            "camera-1",
            "--source-lavfi-video",
            "testsrc2=size=320x240:rate=15",
            "--source-lavfi-audio",
            "sine=frequency=440:sample_rate=48000",
        ])
        .expect("documented lavfi boot flags must parse");
        assert_eq!(args.source_name.as_deref(), Some("camera-1"));
        assert!(args.source.is_none());
        assert_eq!(
            args.source_lavfi_video.as_deref(),
            Some("testsrc2=size=320x240:rate=15")
        );
        assert_eq!(args.preset, QualityPreset::P720);
    }

    #[test]
    fn url_boot_flags_still_parse() {
        let args = parse(&[
            "--tls-generate",
            "localhost",
            "--source-name",
            "camera-1",
            "--source",
            "tests/fixtures/sample.mp4",
        ])
        .expect("url boot flags must parse");
        assert_eq!(args.source.as_deref(), Some("tests/fixtures/sample.mp4"));
    }

    #[test]
    fn source_name_alone_is_rejected() {
        let err = parse(&["--tls-generate", "localhost", "--source-name", "camera-1"])
            .expect_err("--source-name without an input must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("boot_source") || msg.contains("source"),
            "{msg}"
        );
    }

    #[test]
    fn lavfi_and_url_together_are_rejected() {
        parse(&[
            "--tls-generate",
            "localhost",
            "--source-name",
            "camera-1",
            "--source",
            "clip.mp4",
            "--source-lavfi-video",
            "testsrc2=size=320x240:rate=15",
            "--source-lavfi-audio",
            "sine=frequency=440",
        ])
        .expect_err("url and lavfi inputs conflict");
    }

    fn lavfi_args(preset: &str) -> Args {
        parse(&[
            "--tls-generate",
            "localhost",
            "--source-name",
            "lavfi-spike",
            "--preset",
            preset,
            "--source-lavfi-video",
            "testsrc2=size=320x240:rate=15",
            "--source-lavfi-audio",
            "sine=frequency=440:sample_rate=48000",
        ])
        .unwrap_or_else(|err| panic!("lavfi boot with --preset {preset} must parse: {err}"))
    }

    fn value_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .map(String::as_str)
    }

    /// Boot lavfi must carry the chosen preset into the ffmpeg argv.
    /// The spare-port WAN smoke uses `480p`; omitting the flag stays
    /// `720p` so existing boots keep their encode.
    #[test]
    fn lavfi_boot_preset_reaches_ffmpeg_argv() {
        use ts6_media_sidecar::pipeline::ffmpeg_video_args;

        let cases = [
            ("480p", QualityPreset::P480, 854, 480, 24, "1000k"),
            ("720p", QualityPreset::P720, 1280, 720, 30, "2500k"),
            ("1080p", QualityPreset::P1080, 1920, 1080, 30, "4500k"),
            ("480P", QualityPreset::P480, 854, 480, 24, "1000k"),
        ];
        for (flag, preset, w, h, fps, bitrate) in cases {
            let args = lavfi_args(flag);
            let cfg = boot_pipeline_config(&args).expect("lavfi boot config");
            assert_eq!(cfg.preset, preset, "{flag}");
            assert!(
                !cfg.source.pace_input(),
                "lavfi boot must stay unpaced under {flag}"
            );
            let argv = ffmpeg_video_args(&cfg);
            assert_eq!(value_after(&argv, "-b:v"), Some(bitrate), "{flag}");
            assert_eq!(value_after(&argv, "-maxrate"), Some(bitrate), "{flag}");
            let fps_s = fps.to_string();
            assert_eq!(value_after(&argv, "-g"), Some(fps_s.as_str()), "{flag}");
            let vf = value_after(&argv, "-vf").unwrap_or_else(|| panic!("{flag}: {argv:?}"));
            assert!(vf.contains(&format!("fps={fps}")), "{flag}: {vf}");
            assert!(vf.contains(&format!("scale={w}:{h}")), "{flag}: {vf}");
            assert!(vf.contains(&format!("pad={w}:{h}")), "{flag}: {vf}");
            assert_eq!(value_after(&argv, "-f"), Some("lavfi"), "{flag}");
            assert_eq!(
                value_after(&argv, "-i"),
                Some("testsrc2=size=320x240:rate=15"),
                "{flag}"
            );
            assert!(
                !argv.iter().any(|a| a == "-re" || a == "-readrate"),
                "lavfi boot must not gain pacing: {argv:?}"
            );
        }
    }

    #[test]
    fn lavfi_boot_without_preset_encodes_720p() {
        use ts6_media_sidecar::pipeline::ffmpeg_video_args;

        let args = parse(&[
            "--tls-generate",
            "localhost",
            "--source-name",
            "lavfi-spike",
            "--source-lavfi-video",
            "testsrc2=size=320x240:rate=15",
            "--source-lavfi-audio",
            "sine=frequency=440:sample_rate=48000",
        ])
        .expect("lavfi boot without --preset");
        let cfg = boot_pipeline_config(&args).expect("lavfi boot config");
        assert_eq!(cfg.preset, QualityPreset::P720);
        let argv = ffmpeg_video_args(&cfg);
        let vf = value_after(&argv, "-vf").expect("-vf");
        assert!(vf.contains("fps=30"), "{vf}");
        assert!(vf.contains("scale=1280:720"), "{vf}");
        assert_eq!(value_after(&argv, "-b:v"), Some("2500k"));
    }

    #[test]
    fn lavfi_boot_rejects_unknown_preset() {
        let err = parse(&[
            "--tls-generate",
            "localhost",
            "--source-name",
            "lavfi-spike",
            "--preset",
            "4k",
            "--source-lavfi-video",
            "testsrc2=size=320x240:rate=15",
            "--source-lavfi-audio",
            "sine=frequency=440",
        ])
        .expect_err("unknown preset must fail clap");
        let msg = err.to_string();
        assert!(
            msg.contains("480p") && msg.contains("4k"),
            "error should name the bad value and the legal presets: {msg}"
        );
    }

    #[test]
    fn file_boot_preset_keeps_input_pacing() {
        use ts6_media_sidecar::pipeline::ffmpeg_video_args;

        let args = parse(&[
            "--tls-generate",
            "localhost",
            "--source-name",
            "vod-spike",
            "--preset",
            "480p",
            "--source",
            "/srv/ts6-media/inbox/episode.mp4",
        ])
        .expect("file boot");
        let cfg = boot_pipeline_config(&args).expect("file boot config");
        assert_eq!(cfg.preset, QualityPreset::P480);
        assert!(cfg.source.pace_input());
        let argv = ffmpeg_video_args(&cfg);
        let re = argv.iter().position(|a| a == "-re").expect("-re");
        let i = argv.iter().position(|a| a == "-i").expect("-i");
        assert_eq!(re + 1, i, "{argv:?}");
        assert_eq!(argv[i + 1], "/srv/ts6-media/inbox/episode.mp4");
        let vf = value_after(&argv, "-vf").expect("-vf");
        assert!(vf.contains("scale=854:480"), "{vf}");
        assert!(vf.contains("fps=24"), "{vf}");
    }
}
