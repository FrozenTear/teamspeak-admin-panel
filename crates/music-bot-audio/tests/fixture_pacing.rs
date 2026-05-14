//! PURA-119 acceptance test — feed a committed WAV fixture through the full
//! pipeline (ffmpeg → s16le → audiopus → 20 ms pacer) and verify:
//!
//! 1. The expected number of frames for the fixture's duration arrive.
//! 2. `scheduled_at` is drift-free at the pacer level.
//! 3. Consumer-side waiting on `scheduled_at` yields actual arrival inside
//!    ±2 ms tolerance per frame, ±5 ms cumulative — i.e. WS-1's wire-side
//!    pacer can rely on `scheduled_at` for jitter-free voice transmission.
//!
//! Gated behind `#[ignore]` because it shells out to `ffmpeg`. The smoke
//! recipe and CI guidance live in `docs/voice/audio-pipeline.md`.

use std::process::Command;
use std::time::{Duration, Instant};

use music_bot_audio::AudioPipeline;
use music_bot_audio::source::AudioSourceSpec;
use music_bot_audio::types::{OpusApplication, PipelineConfig};

const FIXTURE_PATH: &str = "tests/fixtures/sine_440_1s_mono_48k.wav";
/// 1 second, 20 ms frames → 50.
const EXPECTED_FRAMES: usize = 50;
/// Per-frame jitter tolerance — issue acceptance bar.
const PER_FRAME_TOLERANCE_MS: i64 = 2;
/// Cumulative drift tolerance across the whole stream.
const CUMULATIVE_TOLERANCE_MS: i64 = 5;
/// ffmpeg cold-start frames discarded before measuring tolerance. With pipeline
/// `frame_buffer = 16` the buffer absorbs the warm-up; once it has caught up
/// to wall-clock cadence, the pacer is exact. WS-1 will pre-roll similarly
/// before starting wire transmission.
const WARMUP_FRAMES: usize = 3;

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[tokio::test]
#[ignore = "shells out to ffmpeg; run with `cargo test -p music-bot-audio --test fixture_pacing -- --ignored --nocapture`"]
async fn ffmpeg_fixture_paces_within_tolerance() {
    if !ffmpeg_available() {
        panic!("ffmpeg not on PATH — install ffmpeg and re-run. See docs/voice/audio-pipeline.md.");
    }

    let cfg = PipelineConfig {
        channels: 1,
        bitrate_bps: Some(64_000),
        application: OpusApplication::Audio,
        frame_buffer: 16,
        event_buffer: 16,
    };
    let mut pipeline = AudioPipeline::spawn(
        AudioSourceSpec::Ffmpeg {
            input: FIXTURE_PATH.to_string(),
        },
        cfg,
    )
    .await
    .expect("AudioPipeline::spawn");
    let mut frames = pipeline.take_frames();

    let mut scheduled_at: Vec<Instant> = Vec::new();
    let mut actual_at: Vec<Instant> = Vec::new();
    let mut indices: Vec<u64> = Vec::new();
    while let Some(frame) = frames.recv().await {
        // Wait until the pacer says it's time, then record actual arrival.
        let now = Instant::now();
        if frame.scheduled_at > now {
            tokio::time::sleep_until(frame.scheduled_at.into()).await;
        }
        let actual = Instant::now();
        scheduled_at.push(frame.scheduled_at);
        actual_at.push(actual);
        indices.push(frame.index);
    }

    assert_eq!(
        indices.len(),
        EXPECTED_FRAMES,
        "expected {EXPECTED_FRAMES} frames for a 1 s fixture, got {}",
        indices.len()
    );

    // Indices monotonic; scheduled_at drift-free relative to the first frame
    // (anchor at frame 0 — the pipeline's pacer start instant is set
    // internally and is naturally a few µs after the test's local clock).
    let anchor = scheduled_at[0];
    for (i, (idx, scheduled)) in indices.iter().zip(scheduled_at.iter()).enumerate() {
        assert_eq!(*idx as usize, i, "frame index drift at {i}");
        let expected = Duration::from_millis(20) * i as u32;
        let got = scheduled.duration_since(anchor);
        assert_eq!(
            got, expected,
            "scheduled_at drifted at frame {i}: {got:?} vs {expected:?}"
        );
    }

    // Per-frame tolerance: actual - scheduled within ±2 ms, ignoring the
    // first WARMUP_FRAMES (ffmpeg cold-start; the pipeline buffer covers it).
    let mut max_drift_ms: i64 = 0;
    for (i, (scheduled, actual)) in scheduled_at.iter().zip(actual_at.iter()).enumerate() {
        if i < WARMUP_FRAMES {
            continue;
        }
        let drift = if actual >= scheduled {
            actual.duration_since(*scheduled).as_millis() as i64
        } else {
            -(scheduled.duration_since(*actual).as_millis() as i64)
        };
        assert!(
            drift.abs() <= PER_FRAME_TOLERANCE_MS,
            "frame {i} drift {drift} ms exceeds ±{PER_FRAME_TOLERANCE_MS} ms tolerance",
        );
        if drift.abs() > max_drift_ms {
            max_drift_ms = drift.abs();
        }
    }

    // Cumulative drift: elapsed wall-clock between the first post-warmup
    // frame and the last, vs the expected (frames - WARMUP) * 20ms.
    let measured = EXPECTED_FRAMES - WARMUP_FRAMES;
    let total_actual = actual_at
        .last()
        .unwrap()
        .duration_since(actual_at[WARMUP_FRAMES])
        .as_millis() as i64;
    let total_expected = ((measured - 1) as i64) * 20;
    let total_drift = total_actual - total_expected;
    assert!(
        total_drift.abs() <= CUMULATIVE_TOLERANCE_MS,
        "cumulative drift {total_drift} ms exceeds ±{CUMULATIVE_TOLERANCE_MS} ms tolerance"
    );

    eprintln!(
        "fixture_pacing: {} frames, max per-frame drift {} ms, total drift {} ms",
        indices.len(),
        max_drift_ms,
        total_drift
    );

    pipeline.shutdown().await;
}

/// Cancellation must not leave orphan `ffmpeg` processes hanging. Spawn the
/// pipeline against the local fixture, drop it before EOF, and verify no
/// ffmpeg subprocess is still reading the fixture afterwards.
#[tokio::test]
#[ignore = "shells out to ffmpeg; checks for orphan processes via `pgrep`"]
async fn ffmpeg_subprocess_cleanup_on_cancel() {
    if !ffmpeg_available() {
        panic!("ffmpeg not on PATH");
    }

    // Baseline: any pre-existing ffmpeg+fixture processes (e.g. a previously
    // crashed run) shouldn't fail this test. Snapshot them and subtract.
    let baseline = pgrep_fixture();

    let cfg = PipelineConfig::default();
    let mut pipeline = AudioPipeline::spawn(
        AudioSourceSpec::Ffmpeg {
            input: FIXTURE_PATH.to_string(),
        },
        cfg,
    )
    .await
    .expect("spawn");
    let mut frames = pipeline.take_frames();
    // Drain one frame so ffmpeg has started.
    let _ = frames.recv().await.expect("first frame");
    drop(frames);
    pipeline.shutdown().await;

    // ffmpeg `kill_on_drop(true)` + `FfmpegSource::Drop::start_kill` reap on
    // the next tokio reaper turn. Allow a bit of slack on slow CI.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let after = pgrep_fixture();
    let leaked: Vec<u32> = after
        .into_iter()
        .filter(|p| !baseline.contains(p))
        .collect();
    assert!(
        leaked.is_empty(),
        "orphan ffmpeg subprocess after cancel: pids {leaked:?}"
    );
}

fn pgrep_fixture() -> Vec<u32> {
    let out = Command::new("pgrep")
        .arg("-f")
        .arg("sine_440_1s_mono_48k.wav")
        .output();
    let bytes = match out {
        Ok(o) => o.stdout,
        Err(_) => return Vec::new(),
    };
    String::from_utf8_lossy(&bytes)
        .split_whitespace()
        .filter_map(|s| s.parse::<u32>().ok())
        .collect()
}
