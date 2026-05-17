//! Audio sibling task — PURA-154.
//!
//! Bridges [`music_bot_audio::AudioPipeline`] into the bot actor's
//! connected loop. The bot actor owns `&mut Connection` while it's
//! online and is the only thread allowed to call `Connection::send_audio`
//! (the same borrow-checker dance the WS-4 prototype settled on — see
//! `crates/ts6-voice-prototype/src/main.rs:152`).
//!
//! The seam this module provides:
//!
//! 1. [`start_pipeline`] tears down any existing pipeline, spawns a fresh
//!    [`AudioPipeline`] from an [`AudioSource`], and forwards Opus frames
//!    + pipeline events into the bot actor via a single `mpsc<AudioMsg>`.
//! 2. The connected loop drains [`ActiveAudio::audio_rx`] in its
//!    `tokio::select!` and calls `con.send_audio(pkt)` on every `Frame`.
//! 3. Pause/Resume flip a `tokio::sync::watch` the sibling honours by
//!    parking on `pause_rx.changed()` — that back-pressures the pipeline
//!    naturally (the encoder's `read_samples` stalls on a full channel).
//! 4. Dropping [`ActiveAudio`] aborts both the sibling and the pipeline
//!    worker — clean teardown on `Stop` / `SkipNext` / `Play(replace)`.

use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use tsclientlib::Connection;
use tsproto_packets::packets::{AudioData, CodecType, OutAudio};

use music_bot_audio::source::AudioSourceSpec;
use music_bot_audio::{AudioPipeline, PipelineConfig, PipelineError, PipelineEvent};

use crate::command::AudioSource;

/// Buffer between the audio sibling task and the bot's connected loop.
/// 32 covers ~640 ms at 20 ms cadence — generous headroom for one
/// `con.send_audio` call to land on the wire without falling behind.
const AUDIO_MSG_BUFFER: usize = 32;

/// One message from the audio sibling task to the bot's connected loop.
#[derive(Debug)]
pub(crate) enum AudioMsg {
    /// Opus payload bytes for one 20 ms frame. The connected loop wraps
    /// these in `OutAudio::new(AudioData::C2S { codec: OpusVoice, .. })`.
    Frame(Vec<u8>),
    /// Out-of-band event from the pipeline (ICY `NowPlaying`, warnings,
    /// end-of-stream). The connected loop forwards these onto the bot's
    /// `BotEvent` broadcast.
    PipelineEvent(PipelineEvent),
    /// The sibling task has finished draining frames AND pipeline events.
    /// The connected loop responds by sending voice-stop and (if a queue
    /// head exists) auto-starting the next track.
    Finished,
}

/// Per-bot audio state. The connected loop holds an `Option<ActiveAudio>`;
/// `Some` means a pipeline is currently spawned (frames may or may not be
/// flowing depending on `paused`).
pub(crate) struct ActiveAudio {
    /// Operator-facing label for diagnostics / `tracing::info!` lines.
    /// Not user-visible past logs today.
    #[allow(dead_code)]
    pub source_label: String,
    /// Drained by the connected loop on every `select!` iteration.
    pub audio_rx: mpsc::Receiver<AudioMsg>,
    /// Flipped by `Pause` / `Resume`. The sibling parks on
    /// `pause_rx.changed()` while `*pause_rx.borrow()` is true.
    pub pause: watch::Sender<bool>,
    /// Incremented by the connected loop each time it sends an Opus frame.
    /// Zero at `Finished` means the pipeline produced no audio (e.g. yt-dlp
    /// failed on a YouTube URL and ffmpeg saw empty stdin).
    pub frames_sent: u64,
    /// PURA-330 — pipeline-spawn time. The connected loop logs total
    /// `start_pipeline` → first-Opus-frame-on-wire latency against this so
    /// the `!play` startup delay is attributable end-to-end.
    pub started_at: std::time::Instant,
    /// PURA-314 — last operator-readable pipeline warning (yt-dlp cookie
    /// gate, private/unavailable video, …). Set from
    /// `PipelineEvent::Warning`; used to build a *specific* `AudioFinished`
    /// failure reason when the pipeline produces 0 frames, instead of the
    /// generic "check yt-dlp/ffmpeg logs".
    pub last_diagnostic: Option<String>,
    /// Kept so `Drop` aborts the sibling on teardown. The sibling owns
    /// the [`AudioPipeline`] (whose own `Drop` aborts the worker task),
    /// so this single handle is enough to cancel the whole audio stack.
    _sibling: JoinHandle<()>,
}

impl ActiveAudio {
    /// Toggle pause. `paused = true` parks the sibling; the pipeline
    /// back-pressures naturally as the frame channel fills.
    pub fn set_paused(&self, paused: bool) {
        // `send_replace` ignores the (already-known) old value; we only
        // care that the receiver sees the new state and wakes its
        // `changed()` await.
        let _ = self.pause.send_replace(paused);
    }
}

/// Translate a [`BotEvent`-facing](crate::command::AudioSource) source
/// into the [`AudioPipeline`] factory request the WS-2 crate consumes.
///
/// Convention: a `synthetic://` URL routes to the in-process tone
/// generator. This is a test-only seam (the integration test in
/// `crates/voice/tests/audio_e2e.rs` uses it to drive end-to-end audio
/// without spawning ffmpeg / yt-dlp). Production URLs are HTTP(S), so
/// there is no collision with real sources.
fn source_to_spec(source: &AudioSource) -> (AudioSourceSpec, String) {
    match source {
        AudioSource::Url(u) if u.starts_with("synthetic:") => {
            let SyntheticParams {
                hz,
                duration_ms,
                amplitude,
            } = parse_synthetic_url(u);
            (
                AudioSourceSpec::SyntheticTone {
                    hz,
                    amplitude,
                    duration_ms,
                },
                format!("synthetic({hz:.0}Hz)"),
            )
        }
        AudioSource::Url(u) => (AudioSourceSpec::YtDlp { url: u.clone() }, u.clone()),
        AudioSource::LibraryPath(p) => {
            let input = p.to_string_lossy().into_owned();
            let label = format!("library:{input}");
            (AudioSourceSpec::Ffmpeg { input }, label)
        }
    }
}

struct SyntheticParams {
    hz: f32,
    amplitude: f32,
    duration_ms: Option<u64>,
}

/// Parse `synthetic://?hz=440&duration_ms=500&amplitude=0.5`. Missing
/// keys default to a short audible test tone. `duration_ms=infinite` or
/// `duration_ms=none` requests an unbounded tone — used by manual
/// soak-style probes.
fn parse_synthetic_url(url: &str) -> SyntheticParams {
    let mut hz = 440.0_f32;
    let mut amplitude = 0.5_f32;
    let mut duration_ms: Option<u64> = Some(500);
    if let Some((_, query)) = url.split_once('?') {
        for pair in query.split('&') {
            let Some((k, v)) = pair.split_once('=') else {
                continue;
            };
            match k {
                "hz" => {
                    if let Ok(f) = v.parse::<f32>() {
                        hz = f;
                    }
                }
                "amplitude" => {
                    if let Ok(f) = v.parse::<f32>() {
                        amplitude = f;
                    }
                }
                "duration_ms" => {
                    duration_ms = match v {
                        "infinite" | "none" => None,
                        other => other.parse::<u64>().ok().or(duration_ms),
                    };
                }
                _ => {}
            }
        }
    }
    SyntheticParams {
        hz,
        amplitude,
        duration_ms,
    }
}

/// Spawn the audio pipeline for `source` and the sibling task that
/// drains it. Replaces any existing pipeline (dropping it aborts the
/// previous worker + sibling). Returns the operator-facing source label
/// so the caller can log it.
pub(crate) async fn start_pipeline(
    current: &mut Option<ActiveAudio>,
    source: &AudioSource,
    yt_cookie_file: Option<PathBuf>,
) -> Result<String, PipelineError> {
    // PURA-330 — latency anchor: captured before teardown so the logged
    // `!play` → first-audio span includes the previous pipeline's drop.
    let started_at = std::time::Instant::now();

    // Drop the previous pipeline first. `Option::take` here so the old
    // `ActiveAudio`'s `Drop` runs before we spawn the replacement — the
    // ffmpeg / yt-dlp subprocesses the previous pipeline held are killed
    // synchronously by their owning source's `Drop`.
    *current = None;

    let (spec, label) = source_to_spec(source);
    // PURA-329 / PURA-342 — the paced sibling drains exactly one frame per
    // 20 ms, so the frame channel is the only stall runway between a producer
    // hiccup (network / yt-dlp / ffmpeg) and a gap on the wire. The 8-frame
    // default is just 160 ms; any stall past that underran the channel and
    // crackled.
    //
    // Two regimes need cover:
    //  * Steady state — PURA-329 sized a 2 s mid-stream runway for clean
    //    long-running playback ("sounds good now" on v1.4.4).
    //  * Start-up — the opening seconds of a yt-dlp fetch dump a burst, then
    //    throughput dips while the network connection ramps. A 1 s pre-buffer
    //    (the PURA-329 watermark) drained faster than the fetch refilled it,
    //    underrunning the wire for the first 1–2 s (PURA-342 startup crackle).
    //    The watermark is now 3 s so playback rides out the network ramp.
    //
    // 250 frames = 5 s frame-channel depth; `prebuffer_frames` holds the first
    // 150 (3 s) before playback starts. Cost: up to ~3 s extra before the
    // first frame in the worst case, but ffmpeg decodes far faster than
    // real-time (the watermark fills in well under a second in practice — see
    // PURA-342's `pipeline_prebuffer_full` log), and it is in the noise next
    // to the ~11 s yt-dlp resolve (PURA-330). `frame_buffer >= prebuffer_frames`
    // so `flush_prebuffer` never blocks the worker mid-prebuffer.
    let cfg = PipelineConfig {
        frame_buffer: 250,
        prebuffer_frames: 150,
        yt_cookie_file,
        ..PipelineConfig::default()
    };
    debug!(label = %label, ?cfg, "spawning audio pipeline");
    let mut pipeline = AudioPipeline::spawn(spec, cfg).await?;
    let frames_rx = pipeline.take_frames();
    let events_rx = pipeline.events();

    let (msg_tx, msg_rx) = mpsc::channel(AUDIO_MSG_BUFFER);
    let (pause_tx, pause_rx) = watch::channel(false);
    let sibling = spawn_sibling(pipeline, frames_rx, events_rx, pause_rx, msg_tx);

    *current = Some(ActiveAudio {
        source_label: label.clone(),
        audio_rx: msg_rx,
        pause: pause_tx,
        frames_sent: 0,
        started_at,
        last_diagnostic: None,
        _sibling: sibling,
    });
    Ok(label)
}

/// PURA-342 — how many opening frames the startup-underrun watchdog
/// watches. 250 frames × 20 ms = the first 5 s of playback, which spans the
/// whole reported "first 1–2 s" startup regime with margin.
const STARTUP_WATCH_FRAMES: u64 = 250;

/// PURA-342 — a frame handed to the wire this far past its paced
/// `scheduled_at` slot means the frame channel underran while it was being
/// fetched: the wire just gapped and the gap is audible (crackle). 12 ms is
/// inside one 20 ms frame and comfortably above tokio/OS scheduler wake
/// jitter, so it flags a real stall without false-positiving on noise.
const STARTUP_LATENESS_WARN: Duration = Duration::from_millis(12);

/// PURA-342 — startup-underrun watchdog. The pipeline pre-buffer is sized for
/// steady-state network jitter; the opening seconds of a yt-dlp fetch can
/// still drain it faster than the fetch refills before throughput ramps,
/// gapping the wire (audible startup crackle — distinct from the steady-state
/// crackle PURA-329 fixed). This samples the frame-channel depth and the
/// per-frame lateness over the opening [`STARTUP_WATCH_FRAMES`] and emits a
/// `music_bot_latency` summary — plus a one-shot WARN the moment a frame
/// lands late — so a startup underrun is diagnosable from logs, not just by
/// ear (PURA-329 instrumented neither regime).
struct StartupMonitor {
    /// Shallowest frame-channel depth seen in the watch window.
    min_buffer: usize,
    /// Worst frame lateness seen in the watch window.
    max_lateness: Duration,
    /// Frames that arrived at/past [`STARTUP_LATENESS_WARN`].
    late_frames: u32,
    /// Whether the one-shot underrun WARN has already fired.
    warned: bool,
}

impl StartupMonitor {
    fn new() -> Self {
        Self {
            min_buffer: usize::MAX,
            max_lateness: Duration::ZERO,
            late_frames: 0,
            warned: false,
        }
    }

    /// Record one delivered frame's channel depth + lateness. Returns `true`
    /// once the watch window is complete (the caller then drops the monitor);
    /// the closing summary is logged before that `true` is returned.
    fn observe(&mut self, index: u64, buffered: usize, lateness: Duration) -> bool {
        self.min_buffer = self.min_buffer.min(buffered);
        self.max_lateness = self.max_lateness.max(lateness);
        if lateness >= STARTUP_LATENESS_WARN {
            self.late_frames += 1;
            if !self.warned {
                self.warned = true;
                warn!(
                    target: "music_bot_latency",
                    stage = "startup_underrun",
                    frame_index = index,
                    lateness_ms = lateness.as_millis() as u64,
                    buffered_frames = buffered,
                    "frame delivered late — startup frame-buffer underrun (audible crackle)",
                );
            }
        }
        if index + 1 >= STARTUP_WATCH_FRAMES {
            self.log_summary("window");
            true
        } else {
            false
        }
    }

    /// Emit the closing summary when playback ends before the watch window
    /// completes (track shorter than [`STARTUP_WATCH_FRAMES`]).
    fn finish(mut self) {
        self.log_summary("eos");
    }

    fn log_summary(&mut self, ended: &str) {
        let min_buffer = if self.min_buffer == usize::MAX {
            0
        } else {
            self.min_buffer
        };
        info!(
            target: "music_bot_latency",
            stage = "startup_buffer_summary",
            min_buffer_frames = min_buffer,
            max_lateness_ms = self.max_lateness.as_millis() as u64,
            late_frames = self.late_frames,
            ended,
            "startup frame-buffer watch complete",
        );
    }
}

fn spawn_sibling(
    pipeline: AudioPipeline,
    mut frames_rx: mpsc::Receiver<music_bot_audio::OpusFrame>,
    mut events_rx: broadcast::Receiver<PipelineEvent>,
    mut pause_rx: watch::Receiver<bool>,
    tx: mpsc::Sender<AudioMsg>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Keep `pipeline` alive for the lifetime of the sibling — its
        // `Drop` aborts the worker task. We don't otherwise touch it.
        let _pipeline_guard = pipeline;

        // PURA-342 — startup-underrun watchdog. Live for the opening
        // `STARTUP_WATCH_FRAMES`, then dropped (steady state is PURA-329's
        // domain and already verified clean).
        let mut startup_monitor = Some(StartupMonitor::new());

        loop {
            // Park while paused. `changed()` wakes on every flip,
            // including pause→pause (we just re-loop and re-check).
            while *pause_rx.borrow() {
                if pause_rx.changed().await.is_err() {
                    // Bot dropped the sender — clean exit.
                    return;
                }
            }
            tokio::select! {
                biased;
                frame = frames_rx.recv() => match frame {
                    Some(f) => {
                        // PURA-342 — sample the startup-underrun watchdog
                        // *before* the pacing sleep. `frames_rx.len()` is the
                        // channel depth behind this frame; `lateness` is how
                        // far past its paced slot the frame arrived — non-zero
                        // only when the channel underran and `recv()` had to
                        // block on the producer (a healthy buffered frame pops
                        // instantly, well ahead of `scheduled_at`).
                        if let Some(monitor) = startup_monitor.as_mut() {
                            let buffered = frames_rx.len();
                            let lateness = std::time::Instant::now()
                                .saturating_duration_since(f.scheduled_at);
                            if monitor.observe(f.index, buffered, lateness) {
                                startup_monitor = None;
                            }
                        }
                        // Wall-clock pacing. The pipeline encodes far faster
                        // than real-time and only the small bounded frame
                        // channel throttles it; without waiting for each
                        // frame's `scheduled_at` slot the frames are pushed
                        // onto the wire in bursts and the TS server's jitter
                        // buffer plays them choppy and laggy (PURA-314). The
                        // pacer's `scheduled_at` is the drift-free
                        // `first-frame anchor + index * 20 ms`; `sleep_until`
                        // returns immediately once that slot is in the past.
                        tokio::time::sleep_until(tokio::time::Instant::from_std(
                            f.scheduled_at,
                        ))
                        .await;
                        if tx.send(AudioMsg::Frame(f.bytes)).await.is_err() {
                            return;
                        }
                    }
                    None => break,
                },
                ev = events_rx.recv() => match ev {
                    Ok(e) => {
                        if tx.send(AudioMsg::PipelineEvent(e)).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => continue,
                },
                _ = pause_rx.changed() => {
                    // Loop back; the outer `while *pause_rx.borrow()`
                    // gate will park us again if we are now paused.
                }
            }
        }
        // PURA-342 — playback ended before the startup watch window
        // completed (track shorter than `STARTUP_WATCH_FRAMES`); flush the
        // summary so even brief plays leave a `music_bot_latency` record.
        if let Some(monitor) = startup_monitor.take() {
            monitor.finish();
        }
        // Pipeline drained cleanly — drain any final events without
        // blocking, then send Finished. Best-effort; the bot may have
        // already torn us down.
        while let Ok(e) = events_rx.try_recv() {
            if tx.send(AudioMsg::PipelineEvent(e)).await.is_err() {
                return;
            }
        }
        let _ = tx.send(AudioMsg::Finished).await;
    })
}

/// Send one 20 ms Opus frame on the wire. Wraps the bytes in the C2S
/// `OutAudio` shape the prototype proved against TS6 (codec = OpusVoice,
/// voice-id = 0). Errors are surfaced to the caller so the connected
/// loop can decide whether to keep the bot online.
// `tsclientlib::Error` is 136 B — over clippy's 128 B threshold for
// `result_large_err`. Boxing the upstream error type just to please the
// lint isn't worth the API churn for a single in-crate caller.
#[allow(clippy::result_large_err)]
pub(crate) fn send_opus_frame(con: &mut Connection, opus: &[u8]) -> Result<(), tsclientlib::Error> {
    let pkt = OutAudio::new(&AudioData::C2S {
        id: 0,
        codec: CodecType::OpusVoice,
        data: opus,
    });
    con.send_audio(pkt)
}

/// Best-effort voice-stop = same packet shape with an empty Opus
/// payload. The TS6 server forwards it to in-channel listeners so their
/// jitter buffers can flush cleanly. Errors are logged at `warn!` only
/// — a failed voice-stop never blocks bot shutdown.
pub(crate) fn send_voice_stop(con: &mut Connection) {
    let pkt = OutAudio::new(&AudioData::C2S {
        id: 0,
        codec: CodecType::OpusVoice,
        data: &[],
    });
    if let Err(err) = con.send_audio(pkt) {
        warn!(?err, "voice-stop send_audio failed (non-fatal)");
    }
}

/// Tear down the current pipeline (if any). Returns whether a pipeline
/// was active before the call — callers use that to decide whether to
/// emit `BotEvent::AudioFinished`.
pub(crate) fn tear_down(current: &mut Option<ActiveAudio>) -> bool {
    current.take().is_some()
}

/// Suppress the unused-imports lint when the integration-test path
/// doesn't pull this in. The `Duration` import keeps doc-time intent
/// readable; touching it here keeps clippy quiet for the time being.
#[allow(dead_code)]
fn _doc_anchor(_d: Duration) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn synthetic_url_default_when_no_query() {
        let SyntheticParams {
            hz,
            amplitude,
            duration_ms,
        } = parse_synthetic_url("synthetic://");
        assert_eq!(hz, 440.0);
        assert_eq!(amplitude, 0.5);
        assert_eq!(duration_ms, Some(500));
    }

    #[test]
    fn synthetic_url_parses_query() {
        let SyntheticParams {
            hz,
            amplitude,
            duration_ms,
        } = parse_synthetic_url("synthetic://?hz=880&duration_ms=200&amplitude=0.3");
        assert_eq!(hz, 880.0);
        assert_eq!(amplitude, 0.3);
        assert_eq!(duration_ms, Some(200));
    }

    #[test]
    fn synthetic_url_infinite_duration() {
        let SyntheticParams { duration_ms, .. } =
            parse_synthetic_url("synthetic://?duration_ms=infinite");
        assert_eq!(duration_ms, None);
    }

    #[test]
    fn source_to_spec_url_routes_to_ytdlp() {
        let (spec, label) = source_to_spec(&AudioSource::Url("https://example.com/x.mp3".into()));
        assert!(matches!(spec, AudioSourceSpec::YtDlp { .. }));
        assert_eq!(label, "https://example.com/x.mp3");
    }

    #[test]
    fn source_to_spec_synthetic_routes_to_tone() {
        let (spec, _) = source_to_spec(&AudioSource::Url("synthetic://?hz=440".into()));
        assert!(matches!(spec, AudioSourceSpec::SyntheticTone { .. }));
    }

    #[test]
    fn source_to_spec_library_routes_to_ffmpeg() {
        let (spec, label) = source_to_spec(&AudioSource::LibraryPath(PathBuf::from("a/b.mp3")));
        assert!(matches!(spec, AudioSourceSpec::Ffmpeg { .. }));
        assert!(label.starts_with("library:"));
    }

    /// PURA-342 — a healthy stream delivers every frame ahead of its paced
    /// slot, so the watchdog records zero lateness, never warns, and reports
    /// the channel depth it observed.
    #[test]
    fn startup_monitor_clean_stream_never_warns() {
        let mut m = StartupMonitor::new();
        for index in 0..10u64 {
            // On-time frames pop instantly: lateness 0, channel well-stocked.
            let done = m.observe(index, 200 - index as usize, Duration::ZERO);
            assert!(!done, "10-frame stream never reaches the window boundary");
        }
        assert!(!m.warned, "no late frame ⇒ no underrun WARN");
        assert_eq!(m.late_frames, 0);
        assert_eq!(m.max_lateness, Duration::ZERO);
        assert_eq!(m.min_buffer, 191, "shallowest observed depth is retained");
    }

    /// PURA-342 — a frame past the lateness threshold trips the one-shot
    /// underrun WARN exactly once, while every late frame still counts toward
    /// the summary's `late_frames`.
    #[test]
    fn startup_monitor_flags_underrun_once() {
        let mut m = StartupMonitor::new();
        m.observe(0, 120, Duration::ZERO);
        assert!(!m.warned);
        // Two consecutive late frames — the channel underran.
        m.observe(1, 0, STARTUP_LATENESS_WARN);
        assert!(m.warned, "a late frame must flip the one-shot WARN");
        assert_eq!(m.late_frames, 1);
        m.observe(2, 0, STARTUP_LATENESS_WARN + Duration::from_millis(30));
        assert_eq!(m.late_frames, 2, "every late frame counts");
        assert_eq!(
            m.max_lateness,
            STARTUP_LATENESS_WARN + Duration::from_millis(30),
            "worst lateness is retained for the summary",
        );
    }

    /// PURA-342 — sub-threshold scheduler jitter must not be mistaken for an
    /// underrun; it is recorded in `max_lateness` but raises no WARN.
    #[test]
    fn startup_monitor_ignores_sub_threshold_jitter() {
        let mut m = StartupMonitor::new();
        m.observe(0, 100, STARTUP_LATENESS_WARN - Duration::from_millis(1));
        assert!(!m.warned, "jitter below the threshold is not an underrun");
        assert_eq!(m.late_frames, 0);
        assert!(m.max_lateness > Duration::ZERO, "jitter still recorded");
    }

    /// PURA-342 — `observe` returns `true` exactly at the watch-window
    /// boundary so the sibling drops the monitor and stops sampling.
    #[test]
    fn startup_monitor_completes_at_window_boundary() {
        let mut m = StartupMonitor::new();
        let last = STARTUP_WATCH_FRAMES - 1;
        assert!(
            !m.observe(last - 1, 100, Duration::ZERO),
            "one frame short of the window is not complete",
        );
        assert!(
            m.observe(last, 100, Duration::ZERO),
            "the {STARTUP_WATCH_FRAMES}th frame completes the window",
        );
    }

    /// PURA-314 regression — the sibling task must wait for each frame's
    /// `scheduled_at` slot before forwarding it. Before the fix it forwarded
    /// every frame the instant the pipeline produced it; the pipeline encodes
    /// far faster than real-time, so a whole track was blasted onto the wire
    /// in a sub-second burst, which the TS server's jitter buffer rendered as
    /// laggy, choppy playback. A 200 ms synthetic tone is 10 frames; paced
    /// delivery must span most of that 200 ms, not arrive all at once.
    #[tokio::test]
    async fn sibling_paces_frames_to_wall_clock() {
        let mut pipeline = AudioPipeline::spawn(
            AudioSourceSpec::SyntheticTone {
                hz: 440.0,
                amplitude: 0.5,
                duration_ms: Some(200),
            },
            PipelineConfig::default(),
        )
        .await
        .expect("spawn synthetic pipeline");
        let frames_rx = pipeline.take_frames();
        let events_rx = pipeline.events();
        let (_pause_tx, pause_rx) = watch::channel(false);
        let (msg_tx, mut msg_rx) = mpsc::channel(256);

        let started = std::time::Instant::now();
        let sibling = spawn_sibling(pipeline, frames_rx, events_rx, pause_rx, msg_tx);

        let mut frame_count = 0usize;
        let mut last_frame_at = started;
        while let Some(msg) = msg_rx.recv().await {
            match msg {
                AudioMsg::Frame(_) => {
                    frame_count += 1;
                    last_frame_at = std::time::Instant::now();
                }
                AudioMsg::Finished => break,
                AudioMsg::PipelineEvent(_) => {}
            }
        }
        sibling.await.expect("sibling task join");

        assert!(
            frame_count >= 9,
            "200 ms tone at 20 ms frames should yield ~10 frames, got {frame_count}",
        );
        let span = last_frame_at.duration_since(started);
        assert!(
            span >= Duration::from_millis(120),
            "frames arrived within {span:?} — expected real-time pacing (~180 ms for \
             {frame_count} frames), not an unpaced burst",
        );
    }
}
