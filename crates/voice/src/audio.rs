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

use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
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
    /// PURA-352 — the background `yt-dlp -g` resolve task, kept so `Drop`
    /// aborts it if the track is torn down before resolution finishes.
    /// `None` for sources that need no resolve (library / synthetic) and
    /// for a [`seek_to`] respawn (the input is already resolved).
    _resolve: Option<JoinHandle<()>>,
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
        audio_rx: msg_rx,
        pause: pause_tx,
        frames_sent: 0,
        started_at,
        last_diagnostic: None,
        seek_base_secs,
        seek_input,
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
                        let lateness = std::time::Instant::now()
                            .saturating_duration_since(f.scheduled_at);
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
        // PURA-342 — playback drained cleanly; flush the watchdog summaries
        // (startup + whole-play) so every play leaves a `music_bot_latency`
        // record even when no underrun fired.
        monitor.finish();
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
