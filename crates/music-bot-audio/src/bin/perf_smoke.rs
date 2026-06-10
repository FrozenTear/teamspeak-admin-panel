//! PURA-162 WS-Perf — sustained-load + latency smoke harness.
//!
//! Spawns the music-bot audio pipeline against a synthetic tone (default; no
//! external deps) or an ffmpeg-decodable input, paces frames at wall-clock
//! 20 ms, and records:
//!
//! - per-frame pacer drift (`recv_at - scheduled_at`) histogrammed to
//!   p50/p95/p99/max in milliseconds
//! - resource samples (CPU%, RSS MB, FD count) every `--sample-interval-ms`
//!   from `/proc/self/{stat,statm,fd}`
//! - leak deltas (RSS growth %, FD growth count)
//!
//! Emits a JSON report on stdout and (optionally) to `--output`. Exits with
//! status `0` if all configured budgets pass and `1` if any failed — so
//! WS-Gate can wire this directly into the release-gate check.
//!
//! Why synthetic by default: yt-dlp/ffmpeg add seconds of upstream buffering
//! that is structurally separate from the 20 ms pacing budget the music-bot
//! pipeline owns. The synthetic source removes that variance so this smoke
//! measures the pipeline-internal jitter + the wall-clock pacer's precision.
//! The ffmpeg path is available for runs that want to fold in subprocess
//! cold-start; see `docs/voice/perf-smoke.md` for the recipe.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use music_bot_audio::AudioPipeline;
use music_bot_audio::source::AudioSourceSpec;
use music_bot_audio::types::{OpusApplication, PipelineConfig};
use serde::Serialize;
use tracing::{info, warn};

const FRAMES_PER_SECOND: u64 = 50;
const WARMUP_FRAMES: u64 = 12;

#[derive(Parser, Debug)]
#[command(
    name = "perf-smoke",
    about = "PURA-162 music-bot pipeline perf smoke (latency + sustained load)."
)]
struct Cli {
    /// Run for this many seconds. 60 = fast smoke, 1800 = sustained-load
    /// gate. PURA-162 calls for "≥ 30 minutes" on the sustained track.
    #[arg(long, default_value_t = 60)]
    duration_seconds: u64,

    /// Resource sample cadence. 1000 ms keeps the file shape small and is
    /// plenty granular for leak detection — 30 min × 1 Hz = 1800 samples.
    #[arg(long, default_value_t = 1_000)]
    sample_interval_ms: u64,

    /// Source type. `synthetic` removes external-toolchain variance and is
    /// the default. `ffmpeg` shells out to `ffmpeg -i <input>` and is what
    /// you want when budgets fold in subprocess cold-start. `icy` (THE-972)
    /// runs the real `!radio` path — reqwest ICY GET → splitter → ffmpeg —
    /// against a live radio URL, folding stream connect + probe into the
    /// first-frame numbers.
    #[arg(long, value_enum, default_value_t = SourceKind::Synthetic)]
    source: SourceKind,

    /// Required when `--source ffmpeg`. Anything ffmpeg `-i` accepts.
    #[arg(long)]
    ffmpeg_input: Option<String>,

    /// Required when `--source icy`. A Shoutcast/Icecast stream URL —
    /// pick a fixed known-good station so runs stay comparable.
    #[arg(long)]
    icy_url: Option<String>,

    /// THE-972 — before the main paced run, spawn the pipeline this many
    /// extra times and record spawn → first Opus frame per attempt (the
    /// `!radio` resolve → first-audio path the `music_bot_latency` stage
    /// logs break down). Each probe is torn down after its first frame.
    /// 0 = skip.
    #[arg(long, default_value_t = 0)]
    first_frame_probes: u32,

    /// Opus encoder bitrate.
    #[arg(long, default_value_t = 64_000)]
    bitrate: i32,

    /// Channels (1 = mono, TS6 default).
    #[arg(long, default_value_t = 1)]
    channels: u8,

    /// Frame channel depth. 16 (legacy harness default) keeps the producer
    /// on a short leash; the music bot itself runs 250 (5 s runway, see
    /// `crates/voice/src/audio.rs::pipeline_config`). For network sources,
    /// pass the bot's values so steady-state drift measures what the wire
    /// would see rather than upstream chunk cadence.
    #[arg(long, default_value_t = 16)]
    frame_buffer: usize,

    /// Pre-buffer watermark (frames held before the pacer anchors). 0 =
    /// legacy harness behaviour; the music bot runs 150. Applies to the
    /// main paced run only — first-frame probes always run with 0 so they
    /// measure resolve → first Opus frame, not watermark fill.
    #[arg(long, default_value_t = 0)]
    prebuffer_frames: usize,

    /// Where to write the JSON report. Empty = stdout only.
    #[arg(long)]
    output: Option<PathBuf>,

    /// Pacer drift p99 budget (ms). 15 ms catches a 2–3× regression over
    /// the typical 1–5 ms steady-state floor observed on a contended dev
    /// workstation; tighten on the release host once a baseline lands.
    /// See `docs/voice/0001-latency-budget.md` for the WS-1 jitter target
    /// (±2 ms/frame per-stage; ±5 ms cumulative).
    #[arg(long, default_value_t = 15.0)]
    budget_drift_p99_ms: f64,

    /// Pacer drift max budget (ms), post-warmup. 50 ms is a 2-3×
    /// regression alarm over the typical ~20 ms outlier observed on a
    /// contended dev workstation; a clean release host sits well under
    /// 10 ms. A single ≥ 50 ms spike on a 30-minute run is a real signal
    /// worth investigating.
    #[arg(long, default_value_t = 50.0)]
    budget_drift_max_ms: f64,

    /// CPU% steady-state budget (single-core %). 5% is generous for the
    /// synthetic-tone + Opus encode case on a modern x86 core.
    #[arg(long, default_value_t = 25.0)]
    budget_cpu_percent: f64,

    /// RSS growth budget over the run (%). 15% gives a tiny-process
    /// (~6 MB) ~1 MB headroom for allocator settling while still flagging
    /// a real leak — a 100 MB process at 15% is 15 MB of growth, well
    /// above any reasonable steady-state noise. Anchor is sample
    /// `RSS_WARMUP_SAMPLES` (10 s in), not t = 0, so process startup
    /// allocation doesn't show up as "growth".
    #[arg(long, default_value_t = 15.0)]
    budget_rss_growth_percent: f64,

    /// FD growth budget over the run (count). Anything > 0 is suspicious.
    #[arg(long, default_value_t = 0)]
    budget_fd_growth: i64,

    /// First-frame latency budget (ms), checked against the probe p50 when
    /// `--first-frame-probes` ran, else against the main run's first frame.
    /// 0 = unchecked — network sources vary too much for one universal
    /// default; set it per station once a baseline exists.
    #[arg(long, default_value_t = 0.0)]
    budget_first_frame_ms: f64,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum SourceKind {
    Synthetic,
    Ffmpeg,
    Icy,
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    issue: &'static str,
    git_sha: Option<String>,
    target_arch: &'static str,
    target_os: &'static str,
    started_at_unix_ms: u128,
    duration_seconds: u64,
    source: String,
    bitrate_bps: i32,
    channels: u8,
    frames: FramesSummary,
    first_frame: FirstFrameSummary,
    drift_ms: DriftSummary,
    resources: ResourceSummary,
    samples: Vec<ResourceSample>,
    budgets: BudgetTable,
    pass: bool,
}

#[derive(Serialize)]
struct FramesSummary {
    expected: u64,
    received: u64,
    warmup_skipped: u64,
}

/// THE-972 — spawn → first Opus frame latency. For network sources (`icy`)
/// this folds in connect + ffmpeg probe + first encode, i.e. the user-visible
/// `!radio` → first-audio wait the `music_bot_latency` stage logs attribute.
#[derive(Serialize)]
struct FirstFrameSummary {
    /// First frame of the main paced run.
    main_run_ms: f64,
    /// Per-probe spawn → first frame, in probe order. Empty when
    /// `--first-frame-probes 0`.
    probes_ms: Vec<f64>,
    probe_min_ms: f64,
    probe_p50_ms: f64,
    probe_max_ms: f64,
}

#[derive(Serialize)]
struct DriftSummary {
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    max_ms: f64,
    mean_ms: f64,
    cumulative_drift_ms: f64,
}

#[derive(Serialize)]
struct ResourceSummary {
    rss_start_mb: f64,
    rss_end_mb: f64,
    rss_peak_mb: f64,
    rss_growth_percent: f64,
    fd_start: u64,
    fd_end: u64,
    fd_peak: u64,
    fd_growth: i64,
    cpu_mean_percent: f64,
    cpu_peak_percent: f64,
}

#[derive(Serialize, Clone)]
struct ResourceSample {
    t_seconds: f64,
    rss_mb: f64,
    fds: u64,
    cpu_percent: f64,
}

#[derive(Serialize)]
struct BudgetTable {
    drift_p99_ms: BudgetCheck,
    drift_max_ms: BudgetCheck,
    cpu_percent: BudgetCheck,
    rss_growth_percent: BudgetCheck,
    fd_growth: BudgetCheck,
    /// THE-972 — absent when `--budget-first-frame-ms 0` (unchecked).
    #[serde(skip_serializing_if = "Option::is_none")]
    first_frame_ms: Option<BudgetCheck>,
}

#[derive(Serialize)]
struct BudgetCheck {
    budget: f64,
    actual: f64,
    pass: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,music_bot_audio=info".into()),
        )
        .init();
    let cli = Cli::parse();

    let spec = match cli.source {
        SourceKind::Synthetic => AudioSourceSpec::SyntheticTone {
            hz: 440.0,
            amplitude: 0.4,
            duration_ms: None,
        },
        SourceKind::Ffmpeg => {
            let input = cli
                .ffmpeg_input
                .clone()
                .context("--ffmpeg-input is required when --source ffmpeg")?;
            AudioSourceSpec::Ffmpeg { input }
        }
        SourceKind::Icy => {
            let url = cli
                .icy_url
                .clone()
                .context("--icy-url is required when --source icy")?;
            AudioSourceSpec::IcyRadio { url }
        }
    };

    let cfg = PipelineConfig {
        channels: cli.channels,
        bitrate_bps: Some(cli.bitrate),
        application: OpusApplication::Audio,
        frame_buffer: cli.frame_buffer,
        prebuffer_frames: cli.prebuffer_frames.min(cli.frame_buffer),
        event_buffer: 16,
        yt_cookie_file: None,
    };

    // THE-972 — first-frame probes: spawn → first Opus frame, torn down
    // straight after. Run *before* the main paced run so a station's
    // burst-on-connect behaviour is sampled fresh per attempt.
    let mut probes_ms: Vec<f64> = Vec::with_capacity(cli.first_frame_probes as usize);
    // Probes measure resolve → first Opus frame, so the pre-buffer watermark
    // (a deliberate hold) is forced to 0 regardless of the main run's value.
    let probe_cfg = PipelineConfig {
        prebuffer_frames: 0,
        ..cfg.clone()
    };
    for probe in 0..cli.first_frame_probes {
        let t0 = Instant::now();
        let mut p = AudioPipeline::spawn(spec.clone(), probe_cfg.clone())
            .await
            .context("AudioPipeline::spawn (probe)")?;
        let mut probe_frames = p.take_frames();
        match probe_frames.recv().await {
            Some(_) => {
                let ms = t0.elapsed().as_secs_f64() * 1000.0;
                info!(probe, first_frame_ms = ms, "first-frame probe");
                probes_ms.push(ms);
            }
            None => warn!(probe, "first-frame probe yielded no frame"),
        }
        drop(probe_frames);
        p.shutdown().await;
    }

    // THE-986 — the pipeline emits PCM; the production consumer (the voice
    // sibling) applies gain + encodes at dequeue. Mirror that here so the
    // harness measures the same per-frame work the shipped bot does.
    let mut encoder =
        music_bot_audio::OpusFrameEncoder::new(&cfg).context("OpusFrameEncoder::new")?;
    let mut gain = music_bot_audio::GainStage::new(music_bot_audio::VolumeHandle::default());

    let spawn_t0 = Instant::now();
    let mut pipeline = AudioPipeline::spawn(spec, cfg)
        .await
        .context("AudioPipeline::spawn")?;
    let mut frames = pipeline.take_frames();

    let stop = Arc::new(AtomicBool::new(false));
    let stop_sampler = stop.clone();
    let sample_interval = Duration::from_millis(cli.sample_interval_ms);
    let started_at_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let started_at = Instant::now();

    let sampler = tokio::spawn(async move {
        let mut samples: Vec<ResourceSample> = Vec::new();
        let mut prev = ProcSnapshot::read().unwrap_or_default();
        let mut prev_at = Instant::now();
        // Skip the initial CPU sample (no delta yet).
        loop {
            tokio::time::sleep(sample_interval).await;
            if stop_sampler.load(Ordering::Relaxed) {
                break;
            }
            let now = Instant::now();
            match ProcSnapshot::read() {
                Ok(cur) => {
                    let wall = now.duration_since(prev_at).as_secs_f64().max(1e-9);
                    let ticks =
                        (cur.utime + cur.stime).saturating_sub(prev.utime + prev.stime) as f64;
                    let clk_tck = libc_clk_tck();
                    let cpu_percent = (ticks / clk_tck) / wall * 100.0;
                    let rss_mb = cur.rss_pages as f64 * page_size_mb();
                    let t = now.duration_since(started_at).as_secs_f64();
                    samples.push(ResourceSample {
                        t_seconds: t,
                        rss_mb,
                        fds: cur.fd_count,
                        cpu_percent,
                    });
                    prev = cur;
                    prev_at = now;
                }
                Err(e) => warn!(?e, "proc snapshot read failed"),
            }
        }
        samples
    });

    // Frame consumer — drives the pacer and records drift.
    let consumer_deadline = started_at + Duration::from_secs(cli.duration_seconds);
    let mut drift_ms: Vec<f64> =
        Vec::with_capacity((cli.duration_seconds * FRAMES_PER_SECOND) as usize);
    let mut received: u64 = 0;
    let mut main_first_frame_ms = 0.0_f64;
    while let Some(mut frame) = frames.recv().await {
        if received == 0 {
            // Before the pacer sleep — spawn → first frame *available*.
            main_first_frame_ms = spawn_t0.elapsed().as_secs_f64() * 1000.0;
            info!(first_frame_ms = main_first_frame_ms, "main-run first frame");
        }
        // THE-986 — consumer-side gain + encode, as the production sibling
        // does at dequeue (before the pacing sleep).
        gain.apply(&mut frame.samples, frame.channels);
        if let Err(e) = encoder.encode_frame(&frame.samples) {
            warn!(%e, index = frame.index, "consumer-side encode failed — skipping frame");
            continue;
        }
        let now = Instant::now();
        if frame.scheduled_at > now {
            tokio::time::sleep_until(frame.scheduled_at.into()).await;
        }
        let actual = Instant::now();
        let d = if actual >= frame.scheduled_at {
            actual.duration_since(frame.scheduled_at).as_secs_f64() * 1000.0
        } else {
            -(frame.scheduled_at.duration_since(actual).as_secs_f64() * 1000.0)
        };
        drift_ms.push(d);
        received += 1;
        if received.is_multiple_of(FRAMES_PER_SECOND * 60) {
            info!(
                received,
                last_drift_ms = d,
                elapsed_s = started_at.elapsed().as_secs(),
                "frame milestone"
            );
        }
        if Instant::now() >= consumer_deadline {
            break;
        }
    }

    // Signal sampler to wind down and collect samples.
    stop.store(true, Ordering::Relaxed);
    pipeline.shutdown().await;
    let samples = sampler.await.unwrap_or_default();

    let expected = cli.duration_seconds * FRAMES_PER_SECOND;
    let warmup_skipped = WARMUP_FRAMES.min(received);
    let post_warm: Vec<f64> = drift_ms
        .iter()
        .skip(warmup_skipped as usize)
        .map(|d| d.abs())
        .collect();
    let drift_summary = summarize_drift(&drift_ms, &post_warm);

    let resource_summary = summarize_resources(&samples);

    let first_frame = {
        let mut sorted = probes_ms.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        FirstFrameSummary {
            main_run_ms: main_first_frame_ms,
            probe_min_ms: sorted.first().copied().unwrap_or(0.0),
            probe_p50_ms: percentile(&sorted, 50.0),
            probe_max_ms: sorted.last().copied().unwrap_or(0.0),
            probes_ms,
        }
    };
    // Budget anchor: probe p50 when probes ran (stable against one slow
    // connect), else the main run's single sample.
    let first_frame_actual = if first_frame.probes_ms.is_empty() {
        first_frame.main_run_ms
    } else {
        first_frame.probe_p50_ms
    };

    let budgets = BudgetTable {
        drift_p99_ms: BudgetCheck {
            budget: cli.budget_drift_p99_ms,
            actual: drift_summary.p99_ms,
            pass: drift_summary.p99_ms <= cli.budget_drift_p99_ms,
        },
        drift_max_ms: BudgetCheck {
            budget: cli.budget_drift_max_ms,
            actual: drift_summary.max_ms,
            pass: drift_summary.max_ms <= cli.budget_drift_max_ms,
        },
        cpu_percent: BudgetCheck {
            budget: cli.budget_cpu_percent,
            actual: resource_summary.cpu_mean_percent,
            pass: resource_summary.cpu_mean_percent <= cli.budget_cpu_percent,
        },
        rss_growth_percent: BudgetCheck {
            budget: cli.budget_rss_growth_percent,
            actual: resource_summary.rss_growth_percent,
            pass: resource_summary.rss_growth_percent <= cli.budget_rss_growth_percent,
        },
        fd_growth: BudgetCheck {
            budget: cli.budget_fd_growth as f64,
            actual: resource_summary.fd_growth as f64,
            pass: resource_summary.fd_growth <= cli.budget_fd_growth,
        },
        first_frame_ms: (cli.budget_first_frame_ms > 0.0).then_some(BudgetCheck {
            budget: cli.budget_first_frame_ms,
            actual: first_frame_actual,
            pass: first_frame_actual <= cli.budget_first_frame_ms,
        }),
    };
    let pass = budgets.drift_p99_ms.pass
        && budgets.drift_max_ms.pass
        && budgets.cpu_percent.pass
        && budgets.rss_growth_percent.pass
        && budgets.fd_growth.pass
        && budgets.first_frame_ms.as_ref().is_none_or(|b| b.pass);

    let report = Report {
        schema_version: 1,
        issue: "PURA-162",
        git_sha: std::env::var("PERF_SMOKE_GIT_SHA").ok(),
        target_arch: std::env::consts::ARCH,
        target_os: std::env::consts::OS,
        started_at_unix_ms,
        duration_seconds: cli.duration_seconds,
        source: match cli.source {
            SourceKind::Synthetic => "synthetic".to_string(),
            SourceKind::Ffmpeg => format!(
                "ffmpeg:{}",
                cli.ffmpeg_input.as_deref().unwrap_or("<missing>")
            ),
            SourceKind::Icy => format!("icy:{}", cli.icy_url.as_deref().unwrap_or("<missing>")),
        },
        bitrate_bps: cli.bitrate,
        channels: cli.channels,
        frames: FramesSummary {
            expected,
            received,
            warmup_skipped,
        },
        first_frame,
        drift_ms: drift_summary,
        resources: resource_summary,
        samples,
        budgets,
        pass,
    };

    let json = serde_json::to_string_pretty(&report).context("serialize report")?;
    println!("{json}");
    if let Some(path) = cli.output.as_ref() {
        fs::write(path, &json).with_context(|| format!("write report to {}", path.display()))?;
    }

    if !pass {
        std::process::exit(1);
    }
    Ok(())
}

#[derive(Default, Debug, Clone, Copy)]
struct ProcSnapshot {
    utime: u64,
    stime: u64,
    rss_pages: u64,
    fd_count: u64,
}

impl ProcSnapshot {
    fn read() -> std::io::Result<Self> {
        let stat = fs::read_to_string("/proc/self/stat")?;
        // /proc/self/stat field 2 is `(comm)` which may contain whitespace +
        // parens. Anchor on the last `)` and split the remainder.
        let close = stat
            .rfind(')')
            .ok_or_else(|| std::io::Error::other("malformed /proc/self/stat"))?;
        let tail = &stat[close + 1..];
        let fields: Vec<&str> = tail.split_whitespace().collect();
        // After `(comm)`, field 3 is state, so utime = fields[11] (1-indexed
        // 14 in the kernel spec, minus 3 fields already consumed = index 11
        // when 0-indexed).
        let utime: u64 = fields
            .get(11)
            .and_then(|s| s.parse().ok())
            .unwrap_or_default();
        let stime: u64 = fields
            .get(12)
            .and_then(|s| s.parse().ok())
            .unwrap_or_default();

        let statm = fs::read_to_string("/proc/self/statm")?;
        let rss_pages: u64 = statm
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or_default();

        let fd_count = fs::read_dir("/proc/self/fd")
            .map(|it| it.count() as u64)
            .unwrap_or_default();

        Ok(Self {
            utime,
            stime,
            rss_pages,
            fd_count,
        })
    }
}

fn page_size_mb() -> f64 {
    // SAFETY: sysconf is async-signal-safe and has no Rust side-effects.
    let n = unsafe { libc_sysconf(libc_sc_pagesize()) };
    n.max(4096) as f64 / (1024.0 * 1024.0)
}

fn libc_clk_tck() -> f64 {
    let n = unsafe { libc_sysconf(libc_sc_clk_tck()) };
    n.max(1) as f64
}

// We deliberately don't take a libc crate dep just for two sysconf calls; the
// raw FFI shim keeps the dep graph minimal.
unsafe extern "C" {
    fn sysconf(name: i32) -> i64;
}
unsafe fn libc_sysconf(name: i32) -> i64 {
    unsafe { sysconf(name) }
}
const fn libc_sc_clk_tck() -> i32 {
    2
}
const fn libc_sc_pagesize() -> i32 {
    30
}

fn summarize_drift(raw: &[f64], post_warm_abs: &[f64]) -> DriftSummary {
    if raw.is_empty() {
        return DriftSummary {
            p50_ms: 0.0,
            p95_ms: 0.0,
            p99_ms: 0.0,
            max_ms: 0.0,
            mean_ms: 0.0,
            cumulative_drift_ms: 0.0,
        };
    }
    let cumulative = *raw.last().unwrap();
    let mean = if !post_warm_abs.is_empty() {
        post_warm_abs.iter().sum::<f64>() / post_warm_abs.len() as f64
    } else {
        0.0
    };
    let mut sorted = post_warm_abs.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    DriftSummary {
        p50_ms: percentile(&sorted, 50.0),
        p95_ms: percentile(&sorted, 95.0),
        p99_ms: percentile(&sorted, 99.0),
        max_ms: sorted.last().copied().unwrap_or(0.0),
        mean_ms: mean,
        cumulative_drift_ms: cumulative,
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let n = sorted.len();
    let rank = (p / 100.0) * (n as f64 - 1.0);
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    if lo == hi {
        return sorted[lo];
    }
    let frac = rank - lo as f64;
    sorted[lo] * (1.0 - frac) + sorted[hi] * frac
}

fn summarize_resources(samples: &[ResourceSample]) -> ResourceSummary {
    if samples.is_empty() {
        return ResourceSummary {
            rss_start_mb: 0.0,
            rss_end_mb: 0.0,
            rss_peak_mb: 0.0,
            rss_growth_percent: 0.0,
            fd_start: 0,
            fd_end: 0,
            fd_peak: 0,
            fd_growth: 0,
            cpu_mean_percent: 0.0,
            cpu_peak_percent: 0.0,
        };
    }
    // Anchor leak detection on a post-warmup sample (~10 s in) so allocator
    // settling during the first few seconds doesn't show up as a "leak".
    // For short runs (< 30 samples), fall back to the first sample.
    const RSS_WARMUP_SAMPLES: usize = 10;
    let rss_anchor_idx = RSS_WARMUP_SAMPLES.min(samples.len().saturating_sub(1));
    let rss_start = samples[rss_anchor_idx].rss_mb;
    let rss_end = samples.last().unwrap().rss_mb;
    let rss_peak = samples.iter().map(|s| s.rss_mb).fold(0.0_f64, f64::max);
    let fd_start = samples[rss_anchor_idx].fds;
    let fd_end = samples.last().unwrap().fds;
    let fd_peak = samples.iter().map(|s| s.fds).max().unwrap_or(0);
    let cpu_mean: f64 = samples.iter().map(|s| s.cpu_percent).sum::<f64>() / samples.len() as f64;
    let cpu_peak: f64 = samples
        .iter()
        .map(|s| s.cpu_percent)
        .fold(0.0_f64, f64::max);
    let rss_growth_percent = if rss_start > 0.0 {
        ((rss_end - rss_start) / rss_start) * 100.0
    } else {
        0.0
    };
    ResourceSummary {
        rss_start_mb: rss_start,
        rss_end_mb: rss_end,
        rss_peak_mb: rss_peak,
        rss_growth_percent,
        fd_start,
        fd_end,
        fd_peak,
        fd_growth: fd_end as i64 - fd_start as i64,
        cpu_mean_percent: cpu_mean,
        cpu_peak_percent: cpu_peak,
    }
}
