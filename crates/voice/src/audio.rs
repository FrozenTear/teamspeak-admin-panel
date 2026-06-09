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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::{JoinHandle, block_in_place};
use tracing::{debug, info, warn};
use tsclientlib::Connection;
use tsproto_packets::packets::{AudioData, CodecType, OutAudio};

use music_bot_audio::source::AudioSourceSpec;
use music_bot_audio::{AudioPipeline, PipelineConfig, PipelineError, PipelineEvent, VolumeHandle};

use crate::command::AudioSource;

/// Buffer between the audio sibling task and the bot's connected loop.
/// 32 covers ~640 ms at 20 ms cadence — generous headroom for one
/// `con.send_audio` call to land on the wire without falling behind.
const AUDIO_MSG_BUFFER: usize = 32;

/// One message from the audio sibling task to the bot's connected loop.
#[derive(Debug)]
pub(crate) enum AudioMsg {
    /// Opus payload bytes for one 20 ms frame, plus the instant the audio
    /// sibling handed it onto this mpsc. The connected loop wraps the bytes
    /// in `OutAudio::new(AudioData::C2S { codec: OpusVoice, .. })`.
    /// PURA-389a — `enqueued_at` lets the send path measure how long the
    /// frame waited for the connected loop to poll the audio arm (the
    /// candidate-C "loop deferral" leg of the residual stall).
    Frame { bytes: Bytes, enqueued_at: Instant },
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
    ///
    /// PURA-396 — `Option` so the split-wire-task control loop can
    /// `take()` the receiver and hand it to the wire task (via
    /// `WireCmd::InstallAudio`); the single-loop path leaves it `Some`
    /// and drains it in place. Always `Some` immediately after
    /// [`build_active`].
    pub audio_rx: Option<mpsc::Receiver<AudioMsg>>,
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
    /// PURA-352 — playback offset this pipeline was (re)started at, in
    /// whole seconds. The connected loop reports elapsed playback as
    /// `seek_base_secs + frames_sent / FRAMES_PER_PROGRESS_TICK`, so the
    /// FE progress clock stays correct after a seek. Zero for a normal
    /// start-at-zero play.
    pub seek_base_secs: u64,
    /// PURA-352 — the ffmpeg input a [`seek_to`] respawn decodes from,
    /// without re-running yt-dlp resolution. `Some` once known:
    /// immediately for a library file, or after the background
    /// `yt-dlp -g` resolve completes for a URL source. `None` while a URL
    /// is still resolving (or never, for synthetic / unseekable sources)
    /// — in which case a seek is a graceful no-op. Shared with the
    /// background resolve task, hence `Arc<Mutex<_>>`.
    pub seek_input: Arc<Mutex<Option<String>>>,
    /// PURA-389a — per-frame voice send-path timing accumulator. Fed one
    /// [`SendSample`] per Opus frame by [`send_opus_frame`]; emits the
    /// `audio_send_attribution` / `audio_send_summary` `music_bot_latency`
    /// records that attribute the residual `arm=audio` stall to candidate
    /// A / B / C. Per-play state, so it resets on every track change.
    pub send_monitor: SendTimingMonitor,
    /// PURA-352 / THE-896 — the background `yt-dlp -g` resolve task, kept
    /// so [`Drop`] can `.abort()` it if the track is torn down before
    /// resolution finishes. The abort drops the future, which drops the
    /// [`tokio::process::Child`] inside `resolve_direct_url`, which has
    /// `kill_on_drop(true)` set, so the orphan `yt-dlp -g` gets SIGKILL
    /// instead of running to its 25 s `PROCESS_TIMEOUT`. `None` for
    /// sources that need no resolve (library / synthetic) and for a
    /// [`seek_to`] respawn (the input is already resolved).
    _resolve: Option<JoinHandle<()>>,
    /// The paced sibling task. **Not** aborted on [`Drop`]; it self-cleans
    /// when the [`ActiveAudio`]'s `audio_rx` is dropped above as part of
    /// this struct, which closes the sibling's `msg_tx` and lets it return
    /// normally — flushing the PURA-342 / PURA-408b end-of-play summaries
    /// on the way out. Aborting it would cut those summaries off. The
    /// sibling owns the [`AudioPipeline`], whose own `Drop` aborts the
    /// worker task, so this single handle is enough to keep the audio
    /// stack alive for the duration of one track.
    _sibling: JoinHandle<()>,
}

impl Drop for ActiveAudio {
    /// THE-896 — abort the background `yt-dlp -g` resolve task so it does
    /// not outlive the track. See [`ActiveAudio::_resolve`] for the chain
    /// of drops that ends in the `yt-dlp` SIGKILL. `_sibling` is left to
    /// self-clean via mpsc-close (see [`ActiveAudio::_sibling`]).
    fn drop(&mut self) {
        if let Some(h) = self._resolve.take() {
            h.abort();
        }
    }
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

/// PURA-329 / PURA-342 — pipeline buffering config shared by a normal
/// play and a [`seek_to`] respawn.
///
/// The paced sibling drains exactly one frame per 20 ms, so the frame
/// channel is the only stall runway between a producer hiccup (network /
/// yt-dlp / ffmpeg) and a gap on the wire. The 8-frame default is just
/// 160 ms; any stall past that underran the channel and crackled.
///
/// Two regimes need cover:
///  * Steady state — PURA-329 sized a 2 s mid-stream runway for clean
///    long-running playback ("sounds good now" on v1.4.4).
///  * Start-up — the opening seconds of a yt-dlp fetch dump a burst, then
///    throughput dips while the network connection ramps. A 1 s pre-buffer
///    (the PURA-329 watermark) drained faster than the fetch refilled it,
///    underrunning the wire for the first 1–2 s (PURA-342 startup crackle).
///    The watermark is now 3 s so playback rides out the network ramp.
///
/// 250 frames = 5 s frame-channel depth; `prebuffer_frames` holds the first
/// 150 (3 s) before playback starts. Cost: up to ~3 s extra before the
/// first frame in the worst case, but ffmpeg decodes far faster than
/// real-time (the watermark fills in well under a second in practice — see
/// PURA-342's `pipeline_prebuffer_full` log), and it is in the noise next
/// to the ~11 s yt-dlp resolve (PURA-330). `frame_buffer >= prebuffer_frames`
/// so `flush_prebuffer` never blocks the worker mid-prebuffer.
fn pipeline_config(yt_cookie_file: Option<PathBuf>) -> PipelineConfig {
    PipelineConfig {
        frame_buffer: 250,
        prebuffer_frames: 150,
        yt_cookie_file,
        ..PipelineConfig::default()
    }
}

/// Assemble an [`ActiveAudio`] around a freshly-spawned pipeline: take its
/// frame + event channels, spawn the draining sibling, and wire up the
/// per-bot audio state. Shared by [`start_pipeline`] and [`seek_to`].
fn build_active(
    mut pipeline: AudioPipeline,
    source_label: String,
    started_at: Instant,
    seek_base_secs: u64,
    seek_input: Arc<Mutex<Option<String>>>,
    resolve: Option<JoinHandle<()>>,
) -> ActiveAudio {
    let frames_rx = pipeline.take_frames();
    let events_rx = pipeline.events();
    let (msg_tx, msg_rx) = mpsc::channel(AUDIO_MSG_BUFFER);
    let (pause_tx, pause_rx) = watch::channel(false);
    let sibling = spawn_sibling(pipeline, frames_rx, events_rx, pause_rx, msg_tx);
    ActiveAudio {
        source_label,
        audio_rx: Some(msg_rx),
        pause: pause_tx,
        frames_sent: 0,
        started_at,
        last_diagnostic: None,
        seek_base_secs,
        seek_input,
        send_monitor: SendTimingMonitor::new(),
        _resolve: resolve,
        _sibling: sibling,
    }
}

/// Spawn the audio pipeline for `source` and the sibling task that
/// drains it. Replaces any existing pipeline (dropping it aborts the
/// previous worker + sibling). Returns the operator-facing source label
/// so the caller can log it.
/// `volume` is the bot actor's shared output-gain handle (PURA-351). The
/// same handle is passed to every pipeline the bot spawns, so an operator's
/// volume setting persists across track changes and reconnects and a
/// mid-track change is picked up by the live pipeline without a respawn.
pub(crate) async fn start_pipeline(
    current: &mut Option<ActiveAudio>,
    source: &AudioSource,
    yt_cookie_file: Option<PathBuf>,
    volume: &VolumeHandle,
) -> Result<String, PipelineError> {
    // PURA-330 — latency anchor: captured before teardown so the logged
    // `!play` → first-audio span includes the previous pipeline's drop.
    let started_at = Instant::now();

    // Drop the previous pipeline first. `Option::take` here so the old
    // `ActiveAudio`'s `Drop` runs before we spawn the replacement — the
    // ffmpeg / yt-dlp subprocesses the previous pipeline held are killed
    // synchronously by their owning source's `Drop`.
    *current = None;

    let (spec, label) = source_to_spec(source);
    let cfg = pipeline_config(yt_cookie_file.clone());
    debug!(label = %label, ?cfg, gain = volume.get(), "spawning audio pipeline");
    let pipeline = AudioPipeline::spawn(spec, cfg, volume.clone()).await?;

    // PURA-352 — set up seek retention for the new track. A library file
    // is seekable the moment it starts; a URL source needs a one-off
    // `yt-dlp -g` resolve, kicked off in the background so it never
    // delays first audio. Synthetic test tones are left unseekable.
    let seek_input: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let mut resolve: Option<JoinHandle<()>> = None;
    match source {
        AudioSource::LibraryPath(p) => {
            *seek_input.lock().unwrap() = Some(p.to_string_lossy().into_owned());
        }
        AudioSource::Url(u) if !u.starts_with("synthetic:") => {
            let slot = Arc::clone(&seek_input);
            let url = u.clone();
            resolve = Some(tokio::spawn(async move {
                match music_bot_audio::resolve::resolve_direct_url(&url, yt_cookie_file.as_deref())
                    .await
                {
                    Ok(direct) => {
                        debug!("PURA-352 seek: resolved direct media URL for current track");
                        *slot.lock().unwrap() = Some(direct);
                    }
                    Err(err) => {
                        warn!(
                            ?err,
                            "PURA-352 seek: yt-dlp URL resolve failed — seek unavailable for this track"
                        );
                    }
                }
            }));
        }
        _ => {}
    }

    *current = Some(build_active(
        pipeline,
        label.clone(),
        started_at,
        0,
        seek_input,
        resolve,
    ));
    Ok(label)
}

/// PURA-352 — re-spawn the decoder for the current track at `secs`
/// seconds from its start, reusing the retained seekable input (library
/// path, or the yt-dlp media URL resolved at play time) so no yt-dlp
/// resolution is re-run.
///
/// Returns `Ok(true)` when the pipeline was respawned at the offset, or
/// `Ok(false)` when seeking is not (yet) possible — no pipeline is
/// active, or the URL resolve has not finished. The caller treats
/// `Ok(false)` as a graceful no-op.
pub(crate) async fn seek_to(
    current: &mut Option<ActiveAudio>,
    secs: u64,
    volume: &VolumeHandle,
) -> Result<bool, PipelineError> {
    let Some(active) = current.as_ref() else {
        return Ok(false);
    };
    // The retained input is shared with the background resolve task; clone
    // the `Arc` so it survives the teardown below, and snapshot its value.
    let seek_input = Arc::clone(&active.seek_input);
    let input = seek_input.lock().unwrap().clone();
    let Some(input) = input else {
        return Ok(false);
    };
    let source_label = active.source_label.clone();

    // Drop the current pipeline before spawning the replacement so the old
    // ffmpeg subprocess is killed synchronously — mirrors `start_pipeline`.
    let started_at = Instant::now();
    *current = None;

    let spec = AudioSourceSpec::FfmpegAt {
        input,
        start_secs: secs,
    };
    // The seek path decodes a resolved URL / local file directly — no
    // yt-dlp involvement, so no cookie file is needed.
    let cfg = pipeline_config(None);
    debug!(
        secs,
        gain = volume.get(),
        "PURA-352 seek: re-spawning decoder at offset"
    );
    let pipeline = AudioPipeline::spawn(spec, cfg, volume.clone()).await?;

    *current = Some(build_active(
        pipeline,
        source_label,
        started_at,
        secs,
        seek_input,
        None,
    ));
    Ok(true)
}

/// PURA-342 — how many opening frames count as the "startup" regime.
/// 250 frames × 20 ms = the first 5 s of playback, which spans the whole
/// reported "first 1–2 s" startup window with margin. After this the monitor
/// keeps watching but tags underruns `midsong`.
const STARTUP_WATCH_FRAMES: u64 = 250;

/// PURA-342 — a frame handed to the wire this far past its paced
/// `scheduled_at` slot means delivery stalled somewhere on the path to the
/// wire: the wire just gapped and the gap is audible (crackle). The stall is
/// either the frame channel draining (producer too slow) *or* the connected
/// loop not polling the audio arm in time (consumer starved) — the logged
/// `buffered_frames` distinguishes them. 12 ms is inside one 20 ms frame and
/// comfortably above tokio/OS scheduler wake jitter, so it flags a real stall
/// without false-positiving on noise.
const LATENESS_WARN: Duration = Duration::from_millis(12);

/// PURA-342 — frame-buffer underrun watchdog for the *whole* of a play. The
/// pipeline pre-buffer + frame channel are sized to absorb network-fetch
/// jitter (PURA-329 steady state, PURA-342 startup); when a stall outlasts
/// that runway the channel drains, a frame reaches the wire past its paced
/// slot, and the listener hears a crackle. This samples every delivered
/// frame's lateness + channel depth and emits `music_bot_latency` records so
/// an underrun — startup *or* mid-song — is diagnosable from logs, not just
/// by ear (PURA-329 instrumented neither regime):
///
///  * `startup_buffer_summary` — once, at the end of the opening 5 s.
///  * `playback_buffer_summary` — once, when the play ends.
///  * `frame_underrun` WARN — once per distinct underrun *event* (a
///    contiguous run of late frames), tagged `startup` or `midsong`.
struct PlaybackMonitor {
    /// Frames observed so far — also the next frame's expected index + 1.
    frames: u64,
    /// Shallowest frame-channel depth seen during the startup window.
    startup_min_buffer: usize,
    /// Whether the startup summary has been emitted yet.
    startup_summary_done: bool,
    /// Worst frame lateness seen across the whole play.
    max_lateness: Duration,
    /// Frames that arrived at/past [`LATENESS_WARN`], whole play.
    late_frames: u32,
    /// Distinct underrun events (contiguous late-frame runs), whole play.
    underrun_events: u32,
    /// Whether the previous observed frame was late — for event-edge
    /// detection, so one stall logs one WARN, not one per late frame.
    prev_late: bool,
}

impl PlaybackMonitor {
    fn new() -> Self {
        Self {
            frames: 0,
            startup_min_buffer: usize::MAX,
            startup_summary_done: false,
            max_lateness: Duration::ZERO,
            late_frames: 0,
            underrun_events: 0,
            prev_late: false,
        }
    }

    /// Record one delivered frame's channel depth + lateness.
    fn observe(&mut self, index: u64, buffered: usize, lateness: Duration) {
        self.frames = index + 1;
        self.max_lateness = self.max_lateness.max(lateness);
        let in_startup = index < STARTUP_WATCH_FRAMES;
        if in_startup {
            self.startup_min_buffer = self.startup_min_buffer.min(buffered);
        }
        let late = lateness >= LATENESS_WARN;
        if late {
            self.late_frames += 1;
            if !self.prev_late {
                // Rising edge — the start of a fresh underrun event.
                self.underrun_events += 1;
                warn!(
                    target: "music_bot_latency",
                    stage = "frame_underrun",
                    regime = if in_startup { "startup" } else { "midsong" },
                    frame_index = index,
                    lateness_ms = lateness.as_millis() as u64,
                    buffered_frames = buffered,
                    "frame delivered late — wire-send stall (audible crackle); \
                     check buffered_frames: a high value means the consumer \
                     was starved, not the frame buffer drained",
                );
            }
        }
        self.prev_late = late;
        if !self.startup_summary_done && index + 1 >= STARTUP_WATCH_FRAMES {
            self.log_startup_summary("window");
        }
    }

    /// Emit the closing summaries when the play ends.
    fn finish(mut self) {
        if !self.startup_summary_done {
            // Track ended before the startup window completed (short track).
            self.log_startup_summary("eos");
        }
        info!(
            target: "music_bot_latency",
            stage = "playback_buffer_summary",
            frames = self.frames,
            max_lateness_ms = self.max_lateness.as_millis() as u64,
            late_frames = self.late_frames,
            underrun_events = self.underrun_events,
            "playback frame-buffer watch complete — startup + mid-song",
        );
    }

    fn log_startup_summary(&mut self, ended: &str) {
        self.startup_summary_done = true;
        let min_buffer = if self.startup_min_buffer == usize::MAX {
            0
        } else {
            self.startup_min_buffer
        };
        info!(
            target: "music_bot_latency",
            stage = "startup_buffer_summary",
            min_buffer_frames = min_buffer,
            max_lateness_ms = self.max_lateness.as_millis() as u64,
            late_frames = self.late_frames,
            underrun_events = self.underrun_events,
            ended,
            "startup frame-buffer watch complete",
        );
    }
}

/// PURA-408b — frames between `pacer_wakeup` summary emissions. Matched to
/// [`SEND_SUMMARY_INTERVAL`] (1500 frames ≈ 30 s at the 20 ms cadence) so a
/// `pacer_wakeup` window lines up 1:1 with its `audio_send_summary` window
/// in the logs.
const PACER_SUMMARY_INTERVAL: u64 = SEND_SUMMARY_INTERVAL;

/// PURA-408b — a paced `sleep_until` that returns this far past its
/// `scheduled_at` slot counts as a genuine oversleep. tokio's timer wheel
/// has ~1 ms granularity and the OS adds a little wake jitter; 3 ms clears
/// both, so `overslept_frames` counts real misses, not noise.
const PACER_OVERSLEEP_WARN: Duration = Duration::from_millis(3);

/// PURA-408b — one process-/host-level CPU-contention sample, read once per
/// `pacer_wakeup` window. The window delta of these splits a pacer
/// oversleep into its two causes:
///
///  * **2a — tokio worker-queue contention**: `runqueue_wait_ns`, from
///    `/proc/self/schedstat` field 2 — ns this process was runnable but
///    waiting for a CPU. Rises when the voice runtime's own tasks (or
///    other process threads) crowd the worker the sibling needs.
///  * **2b — host vCPU steal**: `host_steal_jiffies`, from the `/proc/stat`
///    `cpu` aggregate `steal` field — time the hypervisor ran another
///    guest instead of this VM. Rises when the contabo host is oversold.
#[derive(Clone, Copy)]
struct StealSample {
    /// `/proc/stat` `cpu` line `steal` field, in USER_HZ jiffies.
    host_steal_jiffies: u64,
    /// `/proc/self/schedstat` field 2 — ns spent runnable-but-waiting.
    runqueue_wait_ns: u64,
}

impl StealSample {
    /// Host vCPU steal accrued since `prev`, in milliseconds.
    fn host_steal_ms_since(&self, prev: &StealSample) -> u64 {
        jiffies_to_ms(
            self.host_steal_jiffies
                .saturating_sub(prev.host_steal_jiffies),
        )
    }

    /// Process runqueue-wait accrued since `prev`, in milliseconds.
    fn runqueue_wait_ms_since(&self, prev: &StealSample) -> u64 {
        self.runqueue_wait_ns.saturating_sub(prev.runqueue_wait_ns) / 1_000_000
    }
}

/// PURA-408b — convert USER_HZ jiffies to milliseconds.
fn jiffies_to_ms(jiffies: u64) -> u64 {
    // SAFETY: `_SC_CLK_TCK` is a valid sysconf name and the call takes no
    // pointer arguments. A non-positive return (never observed on Linux)
    // falls back to the conventional 100 Hz.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let hz = if hz > 0 { hz as u64 } else { 100 };
    jiffies.saturating_mul(1000) / hz
}

/// PURA-408b — `/proc/stat` aggregate `cpu` line `steal` field, in jiffies.
/// `None` off Linux or on any parse failure.
fn read_proc_stat_steal() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    // `cpu  user nice system idle iowait irq softirq steal guest guest_nice`
    let mut fields = stat.lines().next()?.split_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    // `steal` is the 8th numeric field after the `cpu` label.
    fields.nth(7)?.parse().ok()
}

/// PURA-408b — `/proc/self/schedstat` field 2 (runnable-but-waiting ns).
/// `None` off Linux or on parse failure; a kernel built without
/// `CONFIG_SCHEDSTATS` reports a live `0`, which is fine — the delta is 0.
fn read_self_schedstat_wait() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/schedstat").ok()?;
    // `<time-on-cpu ns> <time-waiting ns> <timeslices>`
    s.split_whitespace().nth(1)?.parse().ok()
}

/// PURA-408b — sample host steal + process runqueue-wait. `None` only when
/// `/proc/stat` is unreadable (non-Linux): without it the 2a/2b split has
/// no baseline. `schedstat` is best-effort and defaults to 0.
fn read_steal_sample() -> Option<StealSample> {
    Some(StealSample {
        host_steal_jiffies: read_proc_stat_steal()?,
        runqueue_wait_ns: read_self_schedstat_wait().unwrap_or(0),
    })
}

/// PURA-408b — pacer-wakeup oversleep tracker for the audio sibling.
///
/// The 20 ms pacing `sleep_until` in [`spawn_sibling`] is meant to return
/// exactly at a frame's `scheduled_at` slot. When the tokio timer or the OS
/// wakes the sibling task *late*, the frame is forwarded past its slot and
/// the wire gaps — the residual `frame_underrun` events that log
/// `buffered_frames ≈ 249` (the frame channel was full, so the producer was
/// never the problem). `frame_underrun` samples lateness *before* this
/// sleep and `dequeue_gap` is stamped *after* it, so neither sees the
/// oversleep itself. This monitor closes that gap.
///
/// Per frame: `pacer_oversleep` = the `sleep_until` return instant minus
/// the requested `scheduled_at`, recorded only for frames that genuinely
/// slept. A frame whose slot had already passed when the sibling reached
/// the sleep returns immediately — that is an already-late frame (cumulative
/// drift), not a timer that overslept, and `frame_underrun` already counts
/// it.
///
/// Per window: a `pacer_wakeup` INFO every [`PACER_SUMMARY_INTERVAL`]
/// frames, carrying the window oversleep max/mean/count plus the host vCPU
/// steal + process runqueue-wait deltas — the (2a) tokio-contention vs (2b)
/// host-steal split (see [`StealSample`]).
struct PacerMonitor {
    /// Frames observed across the whole play.
    total_frames: u64,
    /// Frames observed since the last summary.
    window_frames: u64,
    /// Window frames that genuinely slept (slot still in the future).
    window_slept_frames: u32,
    /// Window frames whose slot had already passed before the sleep.
    window_already_late_frames: u32,
    /// Window slept-frames whose oversleep reached [`PACER_OVERSLEEP_WARN`].
    window_overslept_frames: u32,
    /// Worst single oversleep this window.
    window_max_oversleep: Duration,
    /// Sum of oversleep over slept frames this window — for the window mean.
    window_sum_oversleep: Duration,
    /// Previous steal sample, for the per-window delta. `None` only off
    /// Linux or before the first successful read.
    last_steal: Option<StealSample>,
}

impl PacerMonitor {
    fn new() -> Self {
        Self {
            total_frames: 0,
            window_frames: 0,
            window_slept_frames: 0,
            window_already_late_frames: 0,
            window_overslept_frames: 0,
            window_max_oversleep: Duration::ZERO,
            window_sum_oversleep: Duration::ZERO,
            last_steal: read_steal_sample(),
        }
    }

    /// Record one paced frame. `recv_at` is when the sibling popped the
    /// frame from the channel (before the sleep); `woke_at` is when the
    /// paced `sleep_until(scheduled_at)` actually returned.
    fn observe(&mut self, scheduled_at: Instant, recv_at: Instant, woke_at: Instant) {
        self.total_frames += 1;
        self.window_frames += 1;

        if recv_at < scheduled_at {
            // Genuinely slept — the slot was in the future. Oversleep is how
            // far past the slot the timer actually fired.
            let oversleep = woke_at.saturating_duration_since(scheduled_at);
            self.window_slept_frames += 1;
            self.window_max_oversleep = self.window_max_oversleep.max(oversleep);
            self.window_sum_oversleep += oversleep;
            if oversleep >= PACER_OVERSLEEP_WARN {
                self.window_overslept_frames += 1;
            }
        } else {
            // Slot already in the past — `sleep_until` returned immediately.
            self.window_already_late_frames += 1;
        }

        if self.window_frames >= PACER_SUMMARY_INTERVAL {
            self.log_summary();
            self.reset_window();
        }
    }

    /// Emit the per-window `pacer_wakeup` summary and refresh the steal
    /// baseline for the next window.
    fn log_summary(&mut self) {
        let steal = read_steal_sample();
        let (host_steal_ms, proc_runqueue_wait_ms) = match (self.last_steal, steal) {
            (Some(prev), Some(cur)) => (
                cur.host_steal_ms_since(&prev),
                cur.runqueue_wait_ms_since(&prev),
            ),
            _ => (0, 0),
        };
        let mean_oversleep_us = if self.window_slept_frames > 0 {
            (self.window_sum_oversleep.as_micros() / self.window_slept_frames as u128) as u64
        } else {
            0
        };
        info!(
            target: "music_bot_latency",
            stage = "pacer_wakeup",
            total_frames = self.total_frames,
            window_frames = self.window_frames,
            slept_frames = self.window_slept_frames,
            already_late_frames = self.window_already_late_frames,
            overslept_frames = self.window_overslept_frames,
            max_pacer_oversleep_us = self.window_max_oversleep.as_micros() as u64,
            mean_pacer_oversleep_us = mean_oversleep_us,
            host_steal_ms,
            proc_runqueue_wait_ms,
            "pacer-wakeup timing window — 20 ms tick oversleep + host vCPU \
             steal; oversleep with low host_steal_ms ⇒ tokio worker-queue \
             contention (2a), oversleep tracking host_steal_ms ⇒ host vCPU \
             steal (2b)",
        );
        // Carry the current sample forward; keep the old baseline if this
        // read failed so the next window can still delta against it.
        if steal.is_some() {
            self.last_steal = steal;
        }
    }

    fn reset_window(&mut self) {
        self.window_frames = 0;
        self.window_slept_frames = 0;
        self.window_already_late_frames = 0;
        self.window_overslept_frames = 0;
        self.window_max_oversleep = Duration::ZERO;
        self.window_sum_oversleep = Duration::ZERO;
    }

    /// Flush a final partial-window `pacer_wakeup` summary when the play
    /// ends, so a track shorter than [`PACER_SUMMARY_INTERVAL`] frames still
    /// leaves a record for PURA-408c.
    fn finish(mut self) {
        if self.window_frames > 0 {
            self.log_summary();
        }
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

        // PURA-342 — frame-buffer underrun watchdog. Lives for the whole
        // play: it tags the opening `STARTUP_WATCH_FRAMES` as the `startup`
        // regime and everything after as `midsong`, so an occasional
        // mid-song crackle is diagnosable from logs too.
        let mut monitor = PlaybackMonitor::new();

        // PURA-408b — pacer-wakeup oversleep tracker. Sees how far past its
        // `scheduled_at` slot the paced `sleep_until` below actually woke —
        // the attribution `frame_underrun` and `dequeue_gap` both miss.
        let mut pacer = PacerMonitor::new();

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
                        // PURA-342 — sample the underrun watchdog *before*
                        // the pacing sleep. `frames_rx.len()` is the channel
                        // depth behind this frame; `lateness` is how far past
                        // its paced slot the frame arrived — non-zero only
                        // when the channel underran and `recv()` had to block
                        // on the producer (a healthy buffered frame pops
                        // instantly, well ahead of `scheduled_at`).
                        let buffered = frames_rx.len();
                        let recv_at = std::time::Instant::now();
                        let lateness =
                            recv_at.saturating_duration_since(f.scheduled_at);
                        monitor.observe(f.index, buffered, lateness);
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
                        // PURA-408b — the paced `sleep_until` is meant to
                        // return exactly at `scheduled_at`; sample how far
                        // past it the sibling task actually woke. `recv_at`
                        // tells the monitor whether this frame genuinely
                        // slept or its slot had already passed.
                        pacer.observe(
                            f.scheduled_at,
                            recv_at,
                            std::time::Instant::now(),
                        );
                        // PURA-389a — stamp the hand-off instant so the
                        // connected loop's send path can measure how long
                        // this frame waited for the audio arm to be polled.
                        if tx
                            .send(AudioMsg::Frame {
                                bytes: f.bytes,
                                enqueued_at: std::time::Instant::now(),
                            })
                            .await
                            .is_err()
                        {
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
        // PURA-342 — playback drained cleanly; flush the watchdog summaries
        // (startup + whole-play) so every play leaves a `music_bot_latency`
        // record even when no underrun fired.
        monitor.finish();
        // PURA-408b — flush the final (partial) pacer-wakeup window so a
        // short track still leaves a `pacer_wakeup` record.
        pacer.finish();
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

/// PURA-389a — frames between `audio_send_summary` emissions. 1500 frames
/// at the 20 ms cadence is one summary every ~30 s: fine-grained enough to
/// see a stall cluster across the ~30-min contabo sample without flooding.
const SEND_SUMMARY_INTERVAL: u64 = 1500;

/// PURA-389a — a frame whose observable budget (`dequeue_gap` +
/// `t_blockinplace`) reaches this gets a per-frame `audio_send_attribution`
/// WARN carrying the full A/B/C breakdown. 10 ms matches the connected
/// loop's `LOOP_STALL_WARN` so every `connected_loop_stall arm=audio` has a
/// companion attribution line. The PURA-389 design (tsclientlib `04aa249`
/// source read) predicts a healthy `con.send_audio` is microseconds, so on
/// a clean run this fires only on the residual stalls we are hunting.
const SEND_ATTRIBUTION_WARN: Duration = Duration::from_millis(10);

/// PURA-389a — per-thread consumed CPU time (`CLOCK_THREAD_CPUTIME_ID`).
///
/// Compared with wall time across the `block_in_place` send: `wall ≫ cpu`
/// means the `voice-rt` worker thread spent the gap *off* a CPU — the OS
/// preempted it (candidate B) — rather than actually computing.
fn thread_cpu_now() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a live, writable `timespec`; `CLOCK_THREAD_CPUTIME_ID`
    // is a valid POSIX clock id. On the (Linux-unexpected) non-zero return
    // we fall back to `ZERO`, which simply mutes the candidate-B signal for
    // this one frame instead of corrupting the attribution.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    if rc != 0 {
        return Duration::ZERO;
    }
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// PURA-389a — one Opus frame's send-path timing, fed to
/// [`SendTimingMonitor::observe`].
struct SendSample {
    /// Candidate C — wall the frame waited in the sibling→loop mpsc before
    /// the connected loop polled the audio arm and reached this send.
    dequeue_gap: Duration,
    /// Wall around the inner `con.send_audio` call only.
    t_send: Duration,
    /// Wall around the whole `block_in_place(...)`, entry/exit included.
    t_blockinplace: Duration,
    /// Thread CPU time consumed across the `t_blockinplace` span.
    cpu: Duration,
}

/// PURA-389a — name the largest of the three PURA-389 send-stall
/// candidates for one frame. A heuristic hint only; the raw `*_us` fields
/// on the log record stay authoritative — A and B genuinely overlap when
/// the thread is preempted *inside* the `block_in_place` machinery, and
/// the aggregate sample is what the attribution comment reasons over.
fn dominant_candidate(c: Duration, a: Duration, b: Duration) -> &'static str {
    if c >= a && c >= b {
        "C_loop_deferral"
    } else if b >= a {
        "B_os_preemption"
    } else {
        "A_block_in_place_churn"
    }
}

/// PURA-389a — aggregates per-frame voice send-path timing and attributes
/// the residual `arm=audio` stall across the three PURA-389 candidates:
///
///  * **A — `block_in_place` churn**: `t_blockinplace − t_send`. Wall spent
///    in tokio's `block_in_place` entry/exit machinery, outside the actual
///    `con.send_audio` call.
///  * **B — OS preemption**: `t_blockinplace − cpu`. Send-span wall during
///    which the `voice-rt` thread was not on a CPU.
///  * **C — loop deferral**: `dequeue_gap`. Wall the frame sat in the
///    sibling→loop mpsc waiting for the connected loop to poll the audio
///    arm — the in-crate-visible leg of "flush deferral behind a busy
///    event arm". The deeper `poll_send_acks` flush leg lives inside
///    `tsclientlib` and would need an upstream shim, deliberately left out
///    of PURA-389a scope (moratorium-gated — see the issue).
///
/// Per frame: a WARN (`audio_send_attribution`) on any frame past
/// [`SEND_ATTRIBUTION_WARN`], naming the dominant candidate. Per window: an
/// INFO (`audio_send_summary`) every [`SEND_SUMMARY_INTERVAL`] frames with
/// the window maxes. Both under the `music_bot_latency` target so they
/// line up with `frame_underrun` / `connected_loop_stall`.
pub(crate) struct SendTimingMonitor {
    /// Frames observed across the whole play — also the per-frame index in
    /// the `audio_send_attribution` records.
    total_frames: u64,
    /// Frames observed since the last summary.
    window_frames: u64,
    /// Per-window maxes (reset by [`Self::reset_window`] after a summary).
    window_max_dequeue_gap: Duration,
    window_max_t_send: Duration,
    window_max_t_blockinplace: Duration,
    /// Worst candidate-A churn (`t_blockinplace − t_send`) this window.
    window_max_churn_a: Duration,
    /// Worst candidate-B preemption gap (`t_blockinplace − cpu`) this window.
    window_max_preempt_b: Duration,
    /// Attribution WARNs raised this window.
    window_attributions: u32,
}

impl SendTimingMonitor {
    pub(crate) fn new() -> Self {
        Self {
            total_frames: 0,
            window_frames: 0,
            window_max_dequeue_gap: Duration::ZERO,
            window_max_t_send: Duration::ZERO,
            window_max_t_blockinplace: Duration::ZERO,
            window_max_churn_a: Duration::ZERO,
            window_max_preempt_b: Duration::ZERO,
            window_attributions: 0,
        }
    }

    /// Record one frame's send-path timing: update the window maxes, raise
    /// a per-frame attribution WARN if it stalled, and flush the window
    /// summary every [`SEND_SUMMARY_INTERVAL`] frames.
    fn observe(&mut self, s: SendSample) {
        self.total_frames += 1;
        self.window_frames += 1;

        // A and B are both subsets of the `t_blockinplace` span (a region
        // split vs a thread-state split), so they can overlap; `saturating_sub`
        // keeps a near-zero candidate from underflowing.
        let churn_a = s.t_blockinplace.saturating_sub(s.t_send);
        let preempt_b = s.t_blockinplace.saturating_sub(s.cpu);

        self.window_max_dequeue_gap = self.window_max_dequeue_gap.max(s.dequeue_gap);
        self.window_max_t_send = self.window_max_t_send.max(s.t_send);
        self.window_max_t_blockinplace = self.window_max_t_blockinplace.max(s.t_blockinplace);
        self.window_max_churn_a = self.window_max_churn_a.max(churn_a);
        self.window_max_preempt_b = self.window_max_preempt_b.max(preempt_b);

        if s.dequeue_gap + s.t_blockinplace >= SEND_ATTRIBUTION_WARN {
            self.window_attributions += 1;
            warn!(
                target: "music_bot_latency",
                stage = "audio_send_attribution",
                frame_index = self.total_frames,
                dominant = dominant_candidate(s.dequeue_gap, churn_a, preempt_b),
                c_loop_deferral_us = s.dequeue_gap.as_micros() as u64,
                b_preempt_us = preempt_b.as_micros() as u64,
                a_blockinplace_churn_us = churn_a.as_micros() as u64,
                t_send_us = s.t_send.as_micros() as u64,
                t_blockinplace_us = s.t_blockinplace.as_micros() as u64,
                send_cpu_us = s.cpu.as_micros() as u64,
                "audio send-path stall — A/B/C breakdown for this frame; \
                 `dominant` names the largest PURA-389 candidate, raw \
                 `*_us` fields are authoritative",
            );
        }

        if self.window_frames >= SEND_SUMMARY_INTERVAL {
            self.log_summary();
            self.reset_window();
        }
    }

    fn log_summary(&self) {
        info!(
            target: "music_bot_latency",
            stage = "audio_send_summary",
            total_frames = self.total_frames,
            window_frames = self.window_frames,
            window_attributions = self.window_attributions,
            max_c_loop_deferral_us = self.window_max_dequeue_gap.as_micros() as u64,
            max_b_preempt_us = self.window_max_preempt_b.as_micros() as u64,
            max_a_blockinplace_churn_us = self.window_max_churn_a.as_micros() as u64,
            max_t_send_us = self.window_max_t_send.as_micros() as u64,
            max_t_blockinplace_us = self.window_max_t_blockinplace.as_micros() as u64,
            "audio send-path timing window — per-window maxes for the \
             PURA-389 A/B/C residual-stall attribution",
        );
    }

    fn reset_window(&mut self) {
        self.window_frames = 0;
        self.window_max_dequeue_gap = Duration::ZERO;
        self.window_max_t_send = Duration::ZERO;
        self.window_max_t_blockinplace = Duration::ZERO;
        self.window_max_churn_a = Duration::ZERO;
        self.window_max_preempt_b = Duration::ZERO;
        self.window_attributions = 0;
    }
}

/// Send one 20 ms Opus frame on the wire. Wraps the bytes in the C2S
/// `OutAudio` shape the prototype proved against TS6 (codec = OpusVoice,
/// voice-id = 0). Errors are surfaced to the caller so the connected
/// loop can decide whether to keep the bot online.
///
/// PURA-389a — `enqueued_at` (stamped by the audio sibling) and `monitor`
/// feed the A/B/C send-stall attribution.
///
/// PURA-396 2b — `use_block_in_place` selects whether the (microsecond)
/// `con.send_audio` call is wrapped in `tokio::task::block_in_place`. The
/// single-loop path passes `true` (unchanged behaviour); the split wire
/// task passes `false` — PURA-389a measured the `block_in_place` entry/exit
/// churn as candidate A (~11–18 % of the residual stall budget) and the
/// wire task does nothing but wire I/O, so wrapping a non-blocking call
/// buys only churn. The A/B/C timers stay live either way (with the flag
/// off `block_in_place` they simply measure the bare send wall).
// `tsclientlib::Error` is 136 B — over clippy's 128 B threshold for
// `result_large_err`. Boxing the upstream error type just to please the
// lint isn't worth the API churn for a single in-crate caller.
#[allow(clippy::result_large_err)]
pub(crate) fn send_opus_frame(
    con: &mut Connection,
    opus: &[u8],
    enqueued_at: Instant,
    monitor: &mut SendTimingMonitor,
    use_block_in_place: bool,
) -> Result<(), tsclientlib::Error> {
    let pkt = OutAudio::new(&AudioData::C2S {
        id: 0,
        codec: CodecType::OpusVoice,
        data: opus,
    });

    // PURA-389a candidate C — how long this frame waited in the sibling→
    // loop mpsc before the connected loop polled the audio arm and reached
    // this send. Measured before any send work so it is a clean pre-send
    // figure.
    let dequeue_gap = enqueued_at.elapsed();

    // PURA-389a candidate B — thread CPU consumed vs wall elapsed across the
    // whole send. Snapshot the thread CPU clock just before the send span.
    let cpu_start = thread_cpu_now();

    // `con.send_audio` does synchronous packet framing + (optional)
    // encryption, then a `VecDeque` enqueue — no UDP syscall (PURA-389
    // tsclientlib `04aa249` source read). When `use_block_in_place` is set
    // the call is marked blocking so the voice runtime keeps other tasks
    // scheduled; the split wire task passes `false` (PURA-396 2b).
    //
    // PURA-389a candidate A — `t_blockinplace` wraps the whole
    // (optionally `block_in_place`-wrapped) span; `t_send` wraps only the
    // inner `con.send_audio`. Their difference is the `block_in_place`
    // entry/exit churn — ~zero when `use_block_in_place` is false.
    let blockinplace_start = Instant::now();
    let send_once = || {
        let send_start = Instant::now();
        let result = con.send_audio(pkt);
        (result, send_start.elapsed())
    };
    let (send_result, t_send) = if use_block_in_place {
        block_in_place(send_once)
    } else {
        send_once()
    };
    let t_blockinplace = blockinplace_start.elapsed();
    let cpu = thread_cpu_now().saturating_sub(cpu_start);

    monitor.observe(SendSample {
        dequeue_gap,
        t_send,
        t_blockinplace,
        cpu,
    });

    send_result
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
    /// slot, so the watchdog records zero lateness, no underrun events, and
    /// retains the channel depth it observed.
    #[test]
    fn playback_monitor_clean_stream_never_warns() {
        let mut m = PlaybackMonitor::new();
        for index in 0..10u64 {
            // On-time frames pop instantly: lateness 0, channel well-stocked.
            m.observe(index, 200 - index as usize, Duration::ZERO);
        }
        assert_eq!(m.underrun_events, 0, "no late frame ⇒ no underrun event");
        assert_eq!(m.late_frames, 0);
        assert_eq!(m.max_lateness, Duration::ZERO);
        assert_eq!(m.frames, 10);
        assert_eq!(
            m.startup_min_buffer, 191,
            "shallowest observed depth is retained",
        );
    }

    /// PURA-342 — a contiguous run of late frames is a *single* underrun
    /// event (one WARN), but every late frame still counts toward
    /// `late_frames`; the regime split is by frame index.
    #[test]
    fn playback_monitor_coalesces_one_stall_into_one_event() {
        let mut m = PlaybackMonitor::new();
        m.observe(0, 120, Duration::ZERO);
        // Three consecutive late frames — one stall.
        m.observe(1, 0, LATENESS_WARN);
        m.observe(2, 0, LATENESS_WARN + Duration::from_millis(30));
        m.observe(3, 0, LATENESS_WARN);
        assert_eq!(m.underrun_events, 1, "one contiguous stall ⇒ one event");
        assert_eq!(m.late_frames, 3, "every late frame counts");
        assert_eq!(
            m.max_lateness,
            LATENESS_WARN + Duration::from_millis(30),
            "worst lateness is retained for the summary",
        );
    }

    /// PURA-342 — two stalls separated by a recovered (on-time) frame are two
    /// distinct underrun events; a mid-song stall past the startup window is
    /// still caught.
    #[test]
    fn playback_monitor_counts_separate_stalls() {
        let mut m = PlaybackMonitor::new();
        // Startup-regime stall.
        m.observe(10, 0, LATENESS_WARN);
        // Recovery — frame back on time.
        m.observe(11, 80, Duration::ZERO);
        // Mid-song stall, well past STARTUP_WATCH_FRAMES.
        m.observe(STARTUP_WATCH_FRAMES + 500, 0, LATENESS_WARN);
        assert_eq!(m.underrun_events, 2, "a recovered frame ends the event");
        assert_eq!(m.late_frames, 2);
    }

    /// PURA-342 — sub-threshold scheduler jitter must not be mistaken for an
    /// underrun; it is recorded in `max_lateness` but raises no event.
    #[test]
    fn playback_monitor_ignores_sub_threshold_jitter() {
        let mut m = PlaybackMonitor::new();
        m.observe(0, 100, LATENESS_WARN - Duration::from_millis(1));
        assert_eq!(m.underrun_events, 0, "jitter below the threshold is fine");
        assert_eq!(m.late_frames, 0);
        assert!(m.max_lateness > Duration::ZERO, "jitter still recorded");
    }

    /// PURA-342 — the startup summary is emitted exactly once, at the watch
    /// window boundary, while the monitor keeps observing afterwards.
    #[test]
    fn playback_monitor_startup_summary_fires_at_boundary() {
        let mut m = PlaybackMonitor::new();
        m.observe(STARTUP_WATCH_FRAMES - 2, 100, Duration::ZERO);
        assert!(
            !m.startup_summary_done,
            "one frame short of the window — startup summary not yet emitted",
        );
        m.observe(STARTUP_WATCH_FRAMES - 1, 100, Duration::ZERO);
        assert!(
            m.startup_summary_done,
            "the {STARTUP_WATCH_FRAMES}th frame closes the startup window",
        );
        // The monitor keeps running for the mid-song regime.
        m.observe(STARTUP_WATCH_FRAMES + 1, 100, Duration::ZERO);
        assert_eq!(m.frames, STARTUP_WATCH_FRAMES + 2);
    }

    /// THE-896 regression — dropping an [`ActiveAudio`] must abort the
    /// background `yt-dlp -g` resolve task it owns. Before the fix the
    /// `_resolve` JoinHandle was merely detached on drop, so under
    /// `!skip` spam each replaced track leaked one orphan `yt-dlp -g` for
    /// up to its 25 s `PROCESS_TIMEOUT`. We observe the abort via an
    /// `AbortHandle` snapshot taken before the drop — no need to expose
    /// the private `_resolve` field.
    #[tokio::test]
    async fn drop_aborts_resolve_join_handle() {
        let pipeline = AudioPipeline::spawn(
            AudioSourceSpec::SyntheticTone {
                hz: 440.0,
                amplitude: 0.5,
                duration_ms: Some(50),
            },
            PipelineConfig::default(),
            VolumeHandle::default(),
        )
        .await
        .expect("spawn synthetic pipeline");

        // A stand-in for the real `yt-dlp -g` resolve task: loops well
        // past the test's lifetime, so only an explicit abort cancels it.
        let resolve = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let resolve_abort = resolve.abort_handle();
        assert!(
            !resolve_abort.is_finished(),
            "stub resolve task is running before drop",
        );

        let active = build_active(
            pipeline,
            "synthetic://test".to_string(),
            Instant::now(),
            0,
            Arc::new(Mutex::new(None)),
            Some(resolve),
        );

        drop(active);
        // `JoinHandle::abort` is non-blocking — give the runtime one tick
        // to propagate the cancel to the task.
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert!(
            resolve_abort.is_finished(),
            "Drop must abort the _resolve JoinHandle (THE-896 regression)",
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
            VolumeHandle::default(),
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
                AudioMsg::Frame { .. } => {
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

    /// PURA-389a — `dominant_candidate` names whichever of C / B / A is the
    /// largest for a frame.
    #[test]
    fn dominant_candidate_picks_the_largest() {
        let big = Duration::from_millis(50);
        let small = Duration::from_millis(5);
        assert_eq!(dominant_candidate(big, small, small), "C_loop_deferral");
        assert_eq!(dominant_candidate(small, small, big), "B_os_preemption");
        assert_eq!(
            dominant_candidate(small, big, small),
            "A_block_in_place_churn",
        );
    }

    /// PURA-389a — a frame well under the 10 ms budget raises no
    /// attribution WARN; a stalled frame raises exactly one and the window
    /// maxes retain its figures.
    #[test]
    fn send_monitor_counts_only_stalled_frames() {
        let mut m = SendTimingMonitor::new();
        // Healthy frame — microsecond send, no wait. No attribution.
        m.observe(SendSample {
            dequeue_gap: Duration::from_micros(40),
            t_send: Duration::from_micros(70),
            t_blockinplace: Duration::from_micros(110),
            cpu: Duration::from_micros(100),
        });
        assert_eq!(m.window_attributions, 0, "healthy frame raises nothing");
        assert_eq!(m.total_frames, 1);
        // Stalled frame — 40 ms stuck in the mpsc (candidate C). One WARN.
        m.observe(SendSample {
            dequeue_gap: Duration::from_millis(40),
            t_send: Duration::from_micros(90),
            t_blockinplace: Duration::from_millis(2),
            cpu: Duration::from_micros(150),
        });
        assert_eq!(m.window_attributions, 1, "stalled frame raises one WARN");
        assert_eq!(m.window_max_dequeue_gap, Duration::from_millis(40));
        assert_eq!(m.window_max_t_blockinplace, Duration::from_millis(2));
        // Candidate-B max is `t_blockinplace − cpu` of the stalled frame.
        assert_eq!(
            m.window_max_preempt_b,
            Duration::from_millis(2) - Duration::from_micros(150),
        );
    }

    /// PURA-408b — a frame that slept and woke past `scheduled_at` records
    /// its oversleep; the per-window max/count track it.
    #[test]
    fn pacer_monitor_records_oversleep() {
        let mut p = PacerMonitor::new();
        let base = Instant::now();
        // Slot 1 s out; popped now (well before it), woke 5 ms late.
        let scheduled = base + Duration::from_secs(1);
        p.observe(scheduled, base, scheduled + Duration::from_millis(5));
        assert_eq!(p.window_slept_frames, 1);
        assert_eq!(p.window_overslept_frames, 1, "5 ms ≥ the 3 ms warn floor");
        assert_eq!(p.window_max_oversleep, Duration::from_millis(5));
        assert_eq!(p.window_already_late_frames, 0);
    }

    /// PURA-408b — a frame whose slot had already passed when it was popped
    /// is an already-late frame, not a pacer oversleep, and is excluded
    /// from the oversleep stats.
    #[test]
    fn pacer_monitor_excludes_already_late_frames() {
        let mut p = PacerMonitor::new();
        let base = Instant::now();
        // The slot is 1 s behind the pop instant.
        let scheduled = base;
        let recv_at = base + Duration::from_secs(1);
        p.observe(scheduled, recv_at, recv_at);
        assert_eq!(p.window_already_late_frames, 1);
        assert_eq!(p.window_slept_frames, 0, "already-late ⇒ not a slept frame");
        assert_eq!(p.window_overslept_frames, 0);
        assert_eq!(p.window_max_oversleep, Duration::ZERO);
    }

    /// PURA-408b — sub-threshold timer jitter is kept in the oversleep
    /// max/mean but does not count toward `overslept_frames`.
    #[test]
    fn pacer_monitor_ignores_sub_threshold_jitter() {
        let mut p = PacerMonitor::new();
        let base = Instant::now();
        let scheduled = base + Duration::from_secs(1);
        p.observe(scheduled, base, scheduled + Duration::from_millis(1));
        assert_eq!(p.window_slept_frames, 1);
        assert_eq!(p.window_overslept_frames, 0, "1 ms is below the 3 ms floor");
        assert_eq!(p.window_max_oversleep, Duration::from_millis(1));
    }

    /// PURA-408b — the window summary fires every `PACER_SUMMARY_INTERVAL`
    /// frames and clears the per-window state; `total_frames` survives.
    #[test]
    fn pacer_monitor_summary_resets_window() {
        let mut p = PacerMonitor::new();
        let base = Instant::now();
        let scheduled = base + Duration::from_secs(1);
        for _ in 0..PACER_SUMMARY_INTERVAL {
            p.observe(scheduled, base, scheduled + Duration::from_millis(5));
        }
        assert_eq!(p.window_frames, 0, "window resets after a summary");
        assert_eq!(p.window_slept_frames, 0);
        assert_eq!(p.window_max_oversleep, Duration::ZERO);
        assert_eq!(
            p.total_frames, PACER_SUMMARY_INTERVAL,
            "cumulative frame count survives the reset",
        );
    }

    /// PURA-408b — `StealSample` deltas are monotonic and saturate rather
    /// than underflow when a counter appears to move backwards.
    #[test]
    fn pacer_steal_delta_saturates() {
        let prev = StealSample {
            host_steal_jiffies: 100,
            runqueue_wait_ns: 5_000_000,
        };
        let cur = StealSample {
            host_steal_jiffies: 250,
            runqueue_wait_ns: 9_000_000,
        };
        assert_eq!(cur.runqueue_wait_ms_since(&prev), 4, "9 ms − 5 ms");
        // 150 steal jiffies → a non-zero ms figure at any CLK_TCK.
        assert!(cur.host_steal_ms_since(&prev) > 0);
        // A backwards counter saturates to 0, never underflows.
        assert_eq!(prev.host_steal_ms_since(&cur), 0);
        assert_eq!(prev.runqueue_wait_ms_since(&cur), 0);
    }

    /// PURA-389a — the window summary fires every `SEND_SUMMARY_INTERVAL`
    /// frames and resets the per-window state, while the cumulative
    /// `total_frames` counter survives.
    #[test]
    fn send_monitor_summary_resets_window() {
        let mut m = SendTimingMonitor::new();
        for _ in 0..SEND_SUMMARY_INTERVAL {
            m.observe(SendSample {
                dequeue_gap: Duration::from_micros(10),
                t_send: Duration::from_micros(20),
                t_blockinplace: Duration::from_micros(30),
                cpu: Duration::from_micros(28),
            });
        }
        assert_eq!(m.window_frames, 0, "window resets after a summary");
        assert_eq!(
            m.total_frames, SEND_SUMMARY_INTERVAL,
            "cumulative frame count survives the reset",
        );
        assert_eq!(
            m.window_max_t_blockinplace,
            Duration::ZERO,
            "per-window maxes are cleared by the reset",
        );
    }
}
