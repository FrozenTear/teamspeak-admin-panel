//! FFmpeg → MoQ media pipeline (WS-2). One [`Pipeline`] owns one logical
//! source. It spawns two FFmpeg subprocesses (video → VP8 in IVF on stdout,
//! audio → Opus in Ogg on stdout), parses frames out of each stream, and
//! writes them as `moq-lite` groups into a per-source [`BroadcastProducer`]
//! registered against the sidecar's [`crate::origin::SidecarOrigin`].
//!
//! Per-source = one [`Pipeline`]; per-codec = one [`FfmpegProcess`].
//! Splitting video and audio into separate FFmpeg invocations doubles the
//! decode cost on the input but lets each subprocess crash + restart
//! independently, and keeps the stdout pipe single-format so the parser
//! doesn't have to demux. Quality presets / unified single-process layout
//! are deferred to WS-4 once we have real numbers to optimise against.
//!
//! The WS-0 reference player (`moq-spike/player/`) subscribes to track
//! name `"video"` and decodes raw VP8 frames. We keep that contract:
//! - Track `video` carries one VP8 frame per `moq-lite` frame. Keyframes
//!   open a new group; inter-frames each get their own group so a slow
//!   subscriber can drop late ones without dropping a keyframe.
//! - Track `audio` carries one Opus packet per `moq-lite` frame, grouped
//!   in batches of [`AUDIO_PACKETS_PER_GROUP`] (= 1s @ 50 packets/s for
//!   20 ms Opus framing). The current WS-0 player does not yet subscribe
//!   to `audio`; the integration smoke does.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use moq_lite::{BroadcastProducer, Track, TrackProducer};
use tokio::io::{AsyncRead, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::origin::SidecarOrigin;
use crate::preset::QualityPreset;

/// Track name carrying VP8 video frames. Must stay `"video"` so the WS-0
/// reference player subscribes without configuration.
pub const TRACK_VIDEO: &str = "video";

/// Track name carrying Opus audio packets.
pub const TRACK_AUDIO: &str = "audio";

/// One MoQ group per N Opus packets. With 20 ms Opus framing this is one
/// group per second, giving a late subscriber a re-sync point at 1 Hz
/// without paying group overhead on every packet.
const AUDIO_PACKETS_PER_GROUP: usize = 50;

/// Min delay before the supervisor restarts a crashed FFmpeg subprocess.
const FFMPEG_RESTART_MIN: Duration = Duration::from_millis(500);

/// Max delay before the supervisor restarts a crashed FFmpeg subprocess.
const FFMPEG_RESTART_MAX: Duration = Duration::from_secs(10);

/// Inputs to [`Pipeline::start`].
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Operator-visible broadcast name. Becomes the `path` argument to
    /// [`SidecarOrigin::register_broadcast`].
    pub name: String,
    /// Source URL/path passed to `ffmpeg -i`. Anything `ffmpeg` accepts
    /// (file path, RTSP/HTTP URL, `-f lavfi -i …`-style inputs, …).
    /// SSRF allow/deny lands in WS-3 (`POST /source`) — this struct just
    /// forwards the string.
    pub source: SourceInput,
    /// Path to the `ffmpeg` binary. Defaults to `ffmpeg` on PATH.
    pub ffmpeg_path: PathBuf,
    /// Quality preset (WS-4 / PURA-142). Drives the video encoder's
    /// resolution / framerate / bitrate per spec §23.4. Immutable for
    /// the life of the pipeline — switching presets requires
    /// `POST /source/stop` + `POST /source`. Defaults to
    /// [`QualityPreset::DEFAULT`] (= `720p`) when callers don't set
    /// one explicitly.
    pub preset: QualityPreset,
}

/// What FFmpeg should read from.
///
/// - `Url` — the same URL feeds both video and audio FFmpeg subprocesses.
///   Anything `ffmpeg -i` accepts: file paths, RTSP/HTTP URLs, etc.
/// - `Lavfi { video, audio }` — synthetic sources (e.g. `testsrc2=...`,
///   `sine=...`). The video subprocess gets the video spec only; the
///   audio subprocess gets the audio spec only.
#[derive(Debug, Clone)]
pub enum SourceInput {
    Url(String),
    Lavfi { video: String, audio: String },
}

#[derive(Debug, Clone, Copy)]
enum TrackKind {
    Video,
    Audio,
}

impl SourceInput {
    fn args_for(&self, kind: TrackKind) -> Vec<String> {
        match self {
            SourceInput::Url(url) => vec!["-i".into(), url.clone()],
            SourceInput::Lavfi { video, audio } => {
                let spec = match kind {
                    TrackKind::Video => video,
                    TrackKind::Audio => audio,
                };
                vec!["-f".into(), "lavfi".into(), "-i".into(), spec.clone()]
            }
        }
    }

    fn display(&self) -> String {
        match self {
            SourceInput::Url(u) => u.clone(),
            SourceInput::Lavfi { video, audio } => format!("lavfi[{video} | {audio}]"),
        }
    }
}

impl PipelineConfig {
    pub fn new(name: impl Into<String>, source: SourceInput) -> Self {
        Self {
            name: name.into(),
            source,
            ffmpeg_path: PathBuf::from("ffmpeg"),
            preset: QualityPreset::DEFAULT,
        }
    }

    pub fn with_ffmpeg_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.ffmpeg_path = path.into();
        self
    }

    pub fn with_preset(mut self, preset: QualityPreset) -> Self {
        self.preset = preset;
        self
    }
}

/// Per-track counters exported via the control-plane `/stats` endpoint
/// (WS-3). Frames + bytes are bumped from the mux loop; `ffmpeg_alive`
/// flips inside the supervisor as the FFmpeg child spawns / exits.
#[derive(Debug, Default)]
pub struct TrackMetrics {
    pub frames_published: AtomicU64,
    pub bytes_published: AtomicU64,
    pub ffmpeg_alive: AtomicBool,
}

#[derive(Debug, Default)]
pub struct PipelineMetrics {
    pub video: TrackMetrics,
    pub audio: TrackMetrics,
}

impl PipelineMetrics {
    fn arc() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

/// Running pipeline. Holds two FFmpeg supervisors; drop to stop both.
pub struct Pipeline {
    name: String,
    preset: QualityPreset,
    origin: Arc<SidecarOrigin>,
    metrics: Arc<PipelineMetrics>,
    _broadcast: BroadcastProducer,
    video: SupervisedTask,
    audio: SupervisedTask,
}

struct SupervisedTask {
    handle: JoinHandle<()>,
    stop: oneshot::Sender<()>,
}

impl SupervisedTask {
    fn abort(self) {
        // Signal cooperative shutdown first so the supervisor can kill its
        // FFmpeg child gracefully; `abort()` as a fallback if the task is
        // wedged.
        let _ = self.stop.send(());
        self.handle.abort();
    }
}

impl Pipeline {
    /// Boot the pipeline. Registers the broadcast against `origin`,
    /// spawns the video + audio FFmpeg supervisors, and returns. The
    /// supervisors run on background tasks owned by the returned handle
    /// and will keep producing frames until [`Pipeline::stop`] is called
    /// (or the pipeline is dropped).
    ///
    /// Returns Err if the broadcast name is already registered or if
    /// track creation fails. FFmpeg spawn errors are handled inside the
    /// supervisors (with restart backoff) — they do not fail
    /// [`Pipeline::start`].
    pub async fn start(config: PipelineConfig, origin: Arc<SidecarOrigin>) -> Result<Self> {
        let mut broadcast = origin
            .register_broadcast(&config.name)
            .await
            .with_context(|| format!("register broadcast '{}'", config.name))?;

        let video_track = broadcast
            .create_track(Track::new(TRACK_VIDEO))
            .with_context(|| format!("create '{}' track on '{}'", TRACK_VIDEO, config.name))?;
        let audio_track = broadcast
            .create_track(Track::new(TRACK_AUDIO))
            .with_context(|| format!("create '{}' track on '{}'", TRACK_AUDIO, config.name))?;

        let metrics = PipelineMetrics::arc();
        let video = spawn_video_supervisor(config.clone(), video_track, metrics.clone());
        let audio = spawn_audio_supervisor(config.clone(), audio_track, metrics.clone());

        let source_display = config.source.display();
        info!(
            broadcast = %config.name,
            source = %source_display,
            preset = %config.preset,
            "pipeline started"
        );

        Ok(Self {
            name: config.name,
            preset: config.preset,
            origin,
            metrics,
            _broadcast: broadcast,
            video,
            audio,
        })
    }

    /// Operator-visible broadcast name (= `source_id` in the WS-3 control
    /// plane).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Quality preset this pipeline was started with. Immutable for the
    /// life of the pipeline (see [`QualityPreset`] docs).
    pub fn preset(&self) -> QualityPreset {
        self.preset
    }

    /// Shared counters touched by the supervisor + mux loops. Cloneable
    /// `Arc` so the control-plane `/stats` handler can read without
    /// taking any of the pipeline's locks.
    pub fn metrics(&self) -> Arc<PipelineMetrics> {
        self.metrics.clone()
    }

    /// Stop both supervisors and unregister the broadcast.
    pub async fn stop(self) {
        self.video.abort();
        self.audio.abort();
        if let Err(err) = self.origin.unregister_broadcast(&self.name).await {
            warn!(broadcast = %self.name, %err, "unregister_broadcast on pipeline stop");
        }
        info!(broadcast = %self.name, "pipeline stopped");
    }
}

// -----------------------------------------------------------------------
// FFmpeg supervisor — spawn, watch, restart with backoff
// -----------------------------------------------------------------------

fn spawn_video_supervisor(
    config: PipelineConfig,
    mut track: TrackProducer,
    metrics: Arc<PipelineMetrics>,
) -> SupervisedTask {
    let (stop_tx, mut stop_rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        let mut state = SupervisorState::new("video", config);
        loop {
            let Some(child) = state.spawn_ffmpeg(ffmpeg_video_args, &mut stop_rx).await else {
                return;
            };
            let Some((stdout, mut child)) = state.prepare(child, &mut stop_rx).await else {
                continue;
            };
            metrics.video.ffmpeg_alive.store(true, Ordering::Relaxed);
            let mux_result = tokio::select! {
                r = mux_video(stdout, &mut track, &metrics.video) => r,
                _ = &mut stop_rx => {
                    debug!(broadcast = %state.name, role = state.role, "stop signalled");
                    let _ = child.kill().await;
                    metrics.video.ffmpeg_alive.store(false, Ordering::Relaxed);
                    return;
                }
            };
            metrics.video.ffmpeg_alive.store(false, Ordering::Relaxed);
            state.finish_iteration(child, mux_result, &mut stop_rx).await;
            if state.stop_requested {
                return;
            }
        }
    });
    SupervisedTask {
        handle,
        stop: stop_tx,
    }
}

fn spawn_audio_supervisor(
    config: PipelineConfig,
    mut track: TrackProducer,
    metrics: Arc<PipelineMetrics>,
) -> SupervisedTask {
    let (stop_tx, mut stop_rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        let mut state = SupervisorState::new("audio", config);
        loop {
            let Some(child) = state.spawn_ffmpeg(ffmpeg_audio_args, &mut stop_rx).await else {
                return;
            };
            let Some((stdout, mut child)) = state.prepare(child, &mut stop_rx).await else {
                continue;
            };
            metrics.audio.ffmpeg_alive.store(true, Ordering::Relaxed);
            let mux_result = tokio::select! {
                r = mux_audio(stdout, &mut track, &metrics.audio) => r,
                _ = &mut stop_rx => {
                    debug!(broadcast = %state.name, role = state.role, "stop signalled");
                    let _ = child.kill().await;
                    metrics.audio.ffmpeg_alive.store(false, Ordering::Relaxed);
                    return;
                }
            };
            metrics.audio.ffmpeg_alive.store(false, Ordering::Relaxed);
            state.finish_iteration(child, mux_result, &mut stop_rx).await;
            if state.stop_requested {
                return;
            }
        }
    });
    SupervisedTask {
        handle,
        stop: stop_tx,
    }
}

/// Per-supervisor scratch state: the broadcast name (for logs), the
/// role label, current backoff, and whether the stop channel fired.
struct SupervisorState {
    role: &'static str,
    name: String,
    ffmpeg_path: PathBuf,
    args_input: PipelineConfig,
    backoff: Duration,
    stop_requested: bool,
}

impl SupervisorState {
    fn new(role: &'static str, config: PipelineConfig) -> Self {
        Self {
            role,
            name: config.name.clone(),
            ffmpeg_path: config.ffmpeg_path.clone(),
            args_input: config,
            backoff: FFMPEG_RESTART_MIN,
            stop_requested: false,
        }
    }

    /// Spawn an `ffmpeg` child. Returns `None` if a stop was requested
    /// during backoff. On spawn failure, sleeps with backoff and tries
    /// again on the next iteration (i.e. returns `Some` with a dummy?).
    /// To keep the supervisor loop linear, this only returns `None` on
    /// permanent stop; spawn errors are caught and the next outer
    /// iteration retries.
    async fn spawn_ffmpeg(
        &mut self,
        args_fn: fn(&PipelineConfig) -> Vec<String>,
        stop_rx: &mut oneshot::Receiver<()>,
    ) -> Option<Child> {
        loop {
            let args = args_fn(&self.args_input);
            info!(broadcast = %self.name, role = self.role, ffmpeg_args = ?args, "spawning ffmpeg");
            let spawn = Command::new(&self.ffmpeg_path)
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn();
            match spawn {
                Ok(child) => return Some(child),
                Err(err) => {
                    warn!(broadcast = %self.name, role = self.role, %err, "ffmpeg spawn failed; backing off");
                    if wait_or_stop(self.backoff, stop_rx).await {
                        self.stop_requested = true;
                        return None;
                    }
                    self.backoff = next_backoff(self.backoff);
                }
            }
        }
    }

    /// Pull stdout off the child (forwarding stderr to tracing). Returns
    /// `None` if stdout was not pipeable; the supervisor loop should
    /// continue (retry).
    async fn prepare(
        &mut self,
        mut child: Child,
        stop_rx: &mut oneshot::Receiver<()>,
    ) -> Option<(BufReader<tokio::process::ChildStdout>, Child)> {
        let stdout = match child.stdout.take() {
            Some(s) => BufReader::new(s),
            None => {
                warn!(broadcast = %self.name, role = self.role, "ffmpeg stdout unavailable; restarting");
                let _ = child.kill().await;
                if wait_or_stop(self.backoff, stop_rx).await {
                    self.stop_requested = true;
                }
                self.backoff = next_backoff(self.backoff);
                return None;
            }
        };
        if let Some(stderr) = child.stderr.take() {
            let label = format!("ffmpeg[{}/{}]", self.name, self.role);
            tokio::spawn(forward_stderr(stderr, label));
        }
        Some((stdout, child))
    }

    async fn finish_iteration(
        &mut self,
        mut child: Child,
        mux_result: Result<()>,
        stop_rx: &mut oneshot::Receiver<()>,
    ) {
        let _ = child.kill().await;
        let exit = child.wait().await.ok();
        match mux_result {
            Ok(()) => {
                self.backoff = FFMPEG_RESTART_MIN;
                info!(broadcast = %self.name, role = self.role, ?exit, "ffmpeg finished; restarting");
            }
            Err(err) => {
                warn!(broadcast = %self.name, role = self.role, ?exit, %err, "ffmpeg/mux failed; restarting with backoff");
            }
        }
        if wait_or_stop(self.backoff, stop_rx).await {
            self.stop_requested = true;
            return;
        }
        self.backoff = next_backoff(self.backoff);
    }
}

async fn forward_stderr(stderr: tokio::process::ChildStderr, label: String) {
    let mut reader = BufReader::new(stderr);
    let mut buf = String::new();
    loop {
        buf.clear();
        let line_result = tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut buf).await;
        match line_result {
            Ok(0) => return,
            Ok(_) => {
                let trimmed = buf.trim_end();
                if !trimmed.is_empty() {
                    debug!(target: "ffmpeg", "{} {}", label, trimmed);
                }
            }
            Err(_) => return,
        }
    }
}

async fn wait_or_stop(dur: Duration, stop_rx: &mut oneshot::Receiver<()>) -> bool {
    tokio::select! {
        _ = sleep(dur) => false,
        _ = stop_rx => true,
    }
}

fn next_backoff(d: Duration) -> Duration {
    (d * 2).min(FFMPEG_RESTART_MAX)
}

// -----------------------------------------------------------------------
// FFmpeg argument sets
// -----------------------------------------------------------------------

/// Build the FFmpeg argv for the video subprocess. Resolution, framerate
/// and bitrate come from [`QualityPreset`] (spec §23.4). The filter
/// chain mirrors the spec's letterbox/pillarbox pattern from §24.1.1 so
/// non-conforming inputs are scaled-to-fit instead of cropped or
/// stretched. Keyframe interval (`-g` / `-keyint_min`) is set to one
/// keyframe per second of source video — same trade-off the current
/// WS-2 720p path makes (1 s join latency, low overhead).
///
/// Public for unit testing (the integration test on `control_plane.rs`
/// asserts argv contents reflect the requested preset).
pub fn ffmpeg_video_args(config: &PipelineConfig) -> Vec<String> {
    let preset = config.preset;
    let fps = preset.framerate();
    let bitrate = preset.video_bitrate();
    let vf = format!(
        "fps={fps},scale={w}:{h}:force_original_aspect_ratio=decrease,\
         pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,format=yuv420p",
        w = preset.width(),
        h = preset.height(),
    );

    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
    ];
    args.extend(config.source.args_for(TrackKind::Video));
    args.extend([
        "-an".into(),
        "-vf".into(),
        vf,
        "-c:v".into(),
        "libvpx".into(),
        "-b:v".into(),
        bitrate.into(),
        "-maxrate".into(),
        bitrate.into(),
        "-deadline".into(),
        "realtime".into(),
        "-cpu-used".into(),
        "5".into(),
        "-g".into(),
        fps.to_string(),
        "-keyint_min".into(),
        fps.to_string(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-f".into(),
        "ivf".into(),
        "pipe:1".into(),
    ]);
    args
}

fn ffmpeg_audio_args(config: &PipelineConfig) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
    ];
    args.extend(config.source.args_for(TrackKind::Audio));
    args.extend([
        "-vn".into(),
        "-c:a".into(),
        "libopus".into(),
        "-b:a".into(),
        "64k".into(),
        "-ac".into(),
        "1".into(),
        "-ar".into(),
        "48000".into(),
        "-application".into(),
        "voip".into(),
        "-f".into(),
        "ogg".into(),
        "pipe:1".into(),
    ]);
    args
}

// -----------------------------------------------------------------------
// Streaming IVF parser (video)
// -----------------------------------------------------------------------

/// Mux IVF-framed VP8 from `reader` into `track`. Each VP8 frame is
/// written as a single `moq-lite` frame. Each frame gets its own group:
/// keyframes ensure new subscribers re-sync, inter-frames in their own
/// group so the relay can drop late ones without dropping a keyframe.
async fn mux_video(
    mut reader: BufReader<tokio::process::ChildStdout>,
    track: &mut TrackProducer,
    metrics: &TrackMetrics,
) -> Result<()> {
    // IVF header is 32 bytes; the size at offset 4..6 is the header size
    // (almost always 32). We tolerate larger headers (skip the extra).
    let mut header = [0u8; 32];
    match read_exact_or_eof(&mut reader, &mut header).await? {
        Ok(()) => {}
        Err(()) => {
            // EOF before a complete header — empty stream is OK for
            // some ffmpeg early-exits. Return cleanly so the supervisor
            // restarts.
            debug!("IVF stream EOF before header");
            return Ok(());
        }
    }
    if &header[0..4] != b"DKIF" {
        bail!("IVF: bad magic ({:?})", &header[0..4]);
    }
    let fourcc = &header[8..12];
    if fourcc != b"VP80" {
        bail!(
            "IVF: unsupported codec fourcc '{}'",
            String::from_utf8_lossy(fourcc)
        );
    }
    let header_size = u16::from_le_bytes([header[4], header[5]]) as usize;
    if header_size > 32 {
        let mut extra = vec![0u8; header_size - 32];
        let _ = read_exact_or_eof(&mut reader, &mut extra).await?;
    }

    let mut frame_buf = Vec::with_capacity(64 * 1024);
    let mut frame_header = [0u8; 12];
    loop {
        // Frame header: size (u32 LE) + timestamp (u64 LE).
        match read_exact_or_eof(&mut reader, &mut frame_header).await? {
            Ok(()) => {}
            Err(()) => {
                debug!("IVF stream EOF");
                return Ok(());
            }
        }
        let frame_size = u32::from_le_bytes([
            frame_header[0],
            frame_header[1],
            frame_header[2],
            frame_header[3],
        ]) as usize;
        if frame_size == 0 || frame_size > 8 * 1024 * 1024 {
            bail!("IVF: implausible frame size {frame_size}");
        }
        frame_buf.resize(frame_size, 0);
        if read_exact_or_eof(&mut reader, &mut frame_buf).await?.is_err() {
            bail!("IVF: short read mid-frame ({frame_size} bytes)");
        }

        // Each frame → one group → one frame inside it. append_group
        // gives the next sequence number automatically; that keeps the
        // supervisor restart-safe (continuing the same TrackProducer
        // produces strictly increasing sequence numbers without us
        // tracking state across restarts).
        let mut group = track
            .append_group()
            .context("append video group")?;
        group
            .write_frame(Bytes::copy_from_slice(&frame_buf))
            .context("write video frame")?;
        group.finish().context("finish video group")?;
        metrics.frames_published.fetch_add(1, Ordering::Relaxed);
        metrics
            .bytes_published
            .fetch_add(frame_size as u64, Ordering::Relaxed);
    }
}

// -----------------------------------------------------------------------
// Streaming Ogg-Opus parser (audio)
// -----------------------------------------------------------------------

/// Mux Ogg-encapsulated Opus from `reader` into `track`. Pulls Ogg pages
/// off the stream, extracts each Opus packet (a packet may span segments
/// or pages), then writes packets as `moq-lite` frames. Packets are
/// batched [`AUDIO_PACKETS_PER_GROUP`] per group.
///
/// The two Opus *header* packets (`OpusHead`, `OpusTags`) are skipped —
/// subscribers receive only the encoded audio packets. AudioDecoder is
/// configured externally (no `description` payload required for `opus`).
async fn mux_audio(
    mut reader: BufReader<tokio::process::ChildStdout>,
    track: &mut TrackProducer,
    metrics: &TrackMetrics,
) -> Result<()> {
    let mut parser = OggParser::new();
    let mut buf = [0u8; 8192];
    let mut packets_in_group: usize = 0;
    let mut current_group: Option<moq_lite::GroupProducer> = None;
    let mut header_packets_seen: usize = 0;

    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) => {
                debug!("Ogg stream EOF");
                if let Some(mut g) = current_group.take() {
                    let _ = g.finish();
                }
                return Ok(());
            }
            Ok(n) => n,
            Err(err) => bail!("Ogg stream read error: {err}"),
        };
        parser.feed(&buf[..n]);

        while let Some(packet) = parser
            .next_packet()
            .map_err(|err| anyhow!("Ogg parse error: {err}"))?
        {
            if header_packets_seen < 2 {
                // OpusHead + OpusTags — skip; player decoder is
                // configured out-of-band.
                header_packets_seen += 1;
                continue;
            }

            let group = match current_group {
                Some(ref mut g) => g,
                None => {
                    let g = track
                        .append_group()
                        .context("append audio group")?;
                    current_group = Some(g);
                    current_group.as_mut().unwrap()
                }
            };
            let packet_len = packet.len();
            group
                .write_frame(Bytes::from(packet))
                .context("write audio frame")?;
            metrics.frames_published.fetch_add(1, Ordering::Relaxed);
            metrics
                .bytes_published
                .fetch_add(packet_len as u64, Ordering::Relaxed);
            packets_in_group += 1;

            if packets_in_group >= AUDIO_PACKETS_PER_GROUP {
                if let Some(mut g) = current_group.take() {
                    g.finish().context("finish audio group")?;
                }
                packets_in_group = 0;
            }
        }
    }
}

/// Minimal streaming Ogg page parser. We only need enough to reassemble
/// Opus packets out of pages; we do NOT validate CRC, BOS/EOS flags, etc.
/// (FFmpeg writes well-formed Ogg, and the data never leaves the local
/// pipe.)
struct OggParser {
    /// Raw byte buffer fed by [`Self::feed`].
    bytes: Vec<u8>,
    /// Bytes consumed from `bytes` so far. Reset to 0 each time we
    /// compact.
    cursor: usize,
    /// Pending packet bytes carried across page boundaries when the
    /// previous page ended with a segment of size 255 (= "continued").
    partial_packet: Vec<u8>,
}

impl OggParser {
    fn new() -> Self {
        Self {
            bytes: Vec::with_capacity(32 * 1024),
            cursor: 0,
            partial_packet: Vec::new(),
        }
    }

    fn feed(&mut self, data: &[u8]) {
        self.bytes.extend_from_slice(data);
    }

    /// Try to extract the next complete Opus packet. Returns
    /// `Ok(Some(packet))` if one is available, `Ok(None)` if more bytes
    /// are needed, `Err` on a structural parse error.
    fn next_packet(&mut self) -> std::result::Result<Option<Vec<u8>>, String> {
        loop {
            // Look for the next page header in the buffer.
            let remaining = &self.bytes[self.cursor..];
            if remaining.len() < 27 {
                self.compact();
                return Ok(None);
            }
            if &remaining[0..4] != b"OggS" {
                return Err(format!(
                    "Ogg: bad capture pattern at offset {}: {:?}",
                    self.cursor,
                    &remaining[0..4]
                ));
            }
            let segment_count = remaining[26] as usize;
            let header_size = 27 + segment_count;
            if remaining.len() < header_size {
                self.compact();
                return Ok(None);
            }
            let segment_table = &remaining[27..header_size];
            let body_size: usize = segment_table.iter().map(|b| *b as usize).sum();
            if remaining.len() < header_size + body_size {
                self.compact();
                return Ok(None);
            }
            let body = &remaining[header_size..header_size + body_size];

            // Walk segment table, accumulating segments into the partial
            // packet. Whenever we see a segment < 255 the packet ends.
            let mut body_offset = 0usize;
            let mut completed: Option<Vec<u8>> = None;
            let mut consumed_segments = 0usize;
            for &seg_size in segment_table {
                let seg = &body[body_offset..body_offset + seg_size as usize];
                self.partial_packet.extend_from_slice(seg);
                body_offset += seg_size as usize;
                consumed_segments += 1;

                if seg_size < 255 {
                    // Packet ends here.
                    completed = Some(std::mem::take(&mut self.partial_packet));
                    break;
                }
            }

            if completed.is_some() {
                // We finished a packet partway through this page. Trim
                // the consumed bytes (= page header + body up to the last
                // consumed segment) and remember the rest of this page is
                // still pending. Easiest: keep the page header intact but
                // patch the segment table to drop already-consumed
                // segments. Simpler still: we always finish *one* packet
                // per call. To handle the "multiple packets per page"
                // case, leave the partially-consumed page in place by
                // rewriting it in-place.
                let leftover_segments = segment_count - consumed_segments;
                if leftover_segments == 0 && body_offset == body_size {
                    // Page fully consumed.
                    self.cursor += header_size + body_size;
                } else {
                    // Rewrite the page in-place to elide consumed
                    // segments. This keeps the parser's invariant
                    // ("bytes[cursor..] starts at an Ogg page header")
                    // without allocating.
                    let new_segment_count = leftover_segments;
                    let new_header_size = 27 + new_segment_count;
                    let new_body_size = body_size - body_offset;
                    let new_page_size = new_header_size + new_body_size;

                    // Preserve the original capture pattern + flags, but
                    // overwrite segment_count + segment table + body.
                    let abs = self.cursor;
                    let mut new_page = Vec::with_capacity(new_page_size);
                    new_page.extend_from_slice(&self.bytes[abs..abs + 26]);
                    new_page.push(new_segment_count as u8);
                    new_page.extend_from_slice(&segment_table[consumed_segments..]);
                    new_page.extend_from_slice(&body[body_offset..]);

                    // Replace the old page with the rewritten one.
                    let old_page_size = header_size + body_size;
                    self.bytes.splice(
                        abs..abs + old_page_size,
                        new_page.iter().copied(),
                    );
                }
                return Ok(Some(completed.unwrap()));
            } else {
                // No packet completed on this page — partial_packet
                // carries over into the next page.
                self.cursor += header_size + body_size;
                // Loop again to try the next page.
            }
        }
    }

    fn compact(&mut self) {
        if self.cursor == 0 {
            return;
        }
        // Drop the bytes we've already consumed.
        self.bytes.drain(0..self.cursor);
        self.cursor = 0;
    }
}

// -----------------------------------------------------------------------
// I/O helpers
// -----------------------------------------------------------------------

/// Read exactly `buf.len()` bytes. Returns `Ok(Ok(()))` on success,
/// `Ok(Err(()))` on clean EOF before the first byte, propagates I/O
/// errors. A partial-then-EOF case is reported as `Ok(Err(()))` so the
/// caller can decide whether to treat it as a hard error or a clean
/// shutdown.
async fn read_exact_or_eof<R: AsyncRead + Unpin>(
    reader: &mut R,
    buf: &mut [u8],
) -> Result<std::result::Result<(), ()>> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]).await? {
            0 => {
                return Ok(Err(()));
            }
            n => filled += n,
        }
    }
    Ok(Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal IVF byte sequence with two synthetic VP8 frames
    /// (one "keyframe" and one "inter") and assert the parser yields
    /// both as separate groups.
    #[tokio::test]
    async fn ivf_parser_extracts_frames() {
        // IVF header: DKIF, version=0, header_size=32, fourcc=VP80,
        // width=320, height=240, frame_rate=30, time_scale=1,
        // frame_count=2, unused=0.
        let mut buf = Vec::new();
        buf.extend_from_slice(b"DKIF");
        buf.extend_from_slice(&0u16.to_le_bytes()); // version
        buf.extend_from_slice(&32u16.to_le_bytes()); // header size
        buf.extend_from_slice(b"VP80");
        buf.extend_from_slice(&320u16.to_le_bytes());
        buf.extend_from_slice(&240u16.to_le_bytes());
        buf.extend_from_slice(&30u32.to_le_bytes());
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.extend_from_slice(&2u32.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());

        // Frame 0: 4 bytes, "keyframe" (bit 0 == 0)
        let frame0 = [0x10, 0xaa, 0xbb, 0xcc];
        buf.extend_from_slice(&(frame0.len() as u32).to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes());
        buf.extend_from_slice(&frame0);

        // Frame 1: 3 bytes, "inter"
        let frame1 = [0x11, 0xdd, 0xee];
        buf.extend_from_slice(&(frame1.len() as u32).to_le_bytes());
        buf.extend_from_slice(&1u64.to_le_bytes());
        buf.extend_from_slice(&frame1);

        let (mut tx, rx) = tokio::io::duplex(4096);
        tokio::io::AsyncWriteExt::write_all(&mut tx, &buf)
            .await
            .unwrap();
        drop(tx);

        let mut track = Track::new("video").produce();
        let consumer = track.consume();
        let reader = BufReader::new(rx);

        // Run mux loop in a task because consumer drains in parallel.
        let mux = tokio::spawn(async move {
            // Use the reader directly — mux_video expects a child stdout
            // type so we re-implement the parse loop here against our
            // duplex stream to verify the IVF logic in isolation.
            mux_video_for_test(reader, &mut track).await
        });

        // Drain two groups.
        let mut consumer = consumer;
        let mut group0 = consumer.next_group().await.unwrap().expect("group 0");
        let f0 = group0.read_frame().await.unwrap().expect("frame 0");
        assert_eq!(&f0[..], &frame0);

        let mut group1 = consumer.next_group().await.unwrap().expect("group 1");
        let f1 = group1.read_frame().await.unwrap().expect("frame 1");
        assert_eq!(&f1[..], &frame1);

        mux.await.unwrap().unwrap();
    }

    // Mirror of `mux_video` but generic over the reader type so we can
    // drive it from `tokio::io::DuplexStream` in the unit test.
    async fn mux_video_for_test<R: AsyncRead + Unpin>(
        mut reader: BufReader<R>,
        track: &mut TrackProducer,
    ) -> Result<()> {
        let mut header = [0u8; 32];
        match read_exact_or_eof(&mut reader, &mut header).await? {
            Ok(()) => {}
            Err(()) => return Ok(()),
        }
        assert_eq!(&header[0..4], b"DKIF");
        let header_size = u16::from_le_bytes([header[4], header[5]]) as usize;
        if header_size > 32 {
            let mut extra = vec![0u8; header_size - 32];
            let _ = read_exact_or_eof(&mut reader, &mut extra).await?;
        }
        let mut frame_buf = Vec::new();
        let mut frame_header = [0u8; 12];
        loop {
            match read_exact_or_eof(&mut reader, &mut frame_header).await? {
                Ok(()) => {}
                Err(()) => return Ok(()),
            }
            let frame_size = u32::from_le_bytes([
                frame_header[0],
                frame_header[1],
                frame_header[2],
                frame_header[3],
            ]) as usize;
            frame_buf.resize(frame_size, 0);
            if read_exact_or_eof(&mut reader, &mut frame_buf).await?.is_err() {
                return Ok(());
            }
            let mut group = track.append_group()?;
            group.write_frame(Bytes::copy_from_slice(&frame_buf))?;
            group.finish()?;
        }
    }

    #[test]
    fn ogg_parser_extracts_packets_across_segments() {
        // Build two Ogg pages:
        // - Page 1: one packet (3 segments of [255, 255, 100] → 610 bytes)
        // - Page 2: one short packet (1 segment of [40] → 40 bytes)
        let mut page1 = Vec::new();
        page1.extend_from_slice(b"OggS");
        page1.push(0); // version
        page1.push(0); // header_type
        page1.extend_from_slice(&0u64.to_le_bytes()); // granule
        page1.extend_from_slice(&0u32.to_le_bytes()); // serial
        page1.extend_from_slice(&0u32.to_le_bytes()); // page seq
        page1.extend_from_slice(&0u32.to_le_bytes()); // crc
        page1.push(3); // segment_count
        page1.extend_from_slice(&[255, 255, 100]);
        page1.extend(std::iter::repeat(b'A').take(610));

        let mut page2 = Vec::new();
        page2.extend_from_slice(b"OggS");
        page2.push(0);
        page2.push(0);
        page2.extend_from_slice(&0u64.to_le_bytes());
        page2.extend_from_slice(&0u32.to_le_bytes());
        page2.extend_from_slice(&1u32.to_le_bytes());
        page2.extend_from_slice(&0u32.to_le_bytes());
        page2.push(1);
        page2.push(40);
        page2.extend(std::iter::repeat(b'B').take(40));

        let mut parser = OggParser::new();
        parser.feed(&page1);
        parser.feed(&page2);

        let p0 = parser.next_packet().unwrap().expect("packet 0");
        assert_eq!(p0.len(), 610);
        assert!(p0.iter().all(|&b| b == b'A'));

        let p1 = parser.next_packet().unwrap().expect("packet 1");
        assert_eq!(p1.len(), 40);
        assert!(p1.iter().all(|&b| b == b'B'));

        assert!(parser.next_packet().unwrap().is_none());
    }

    #[test]
    fn ogg_parser_handles_multiple_packets_per_page() {
        // One page with two short packets: segments [10, 20].
        let mut page = Vec::new();
        page.extend_from_slice(b"OggS");
        page.push(0);
        page.push(0);
        page.extend_from_slice(&0u64.to_le_bytes());
        page.extend_from_slice(&0u32.to_le_bytes());
        page.extend_from_slice(&0u32.to_le_bytes());
        page.extend_from_slice(&0u32.to_le_bytes());
        page.push(2);
        page.extend_from_slice(&[10, 20]);
        page.extend(std::iter::repeat(b'X').take(10));
        page.extend(std::iter::repeat(b'Y').take(20));

        let mut parser = OggParser::new();
        parser.feed(&page);

        let p0 = parser.next_packet().unwrap().expect("packet 0");
        assert_eq!(p0.len(), 10);
        assert!(p0.iter().all(|&b| b == b'X'));

        let p1 = parser.next_packet().unwrap().expect("packet 1");
        assert_eq!(p1.len(), 20);
        assert!(p1.iter().all(|&b| b == b'Y'));

        assert!(parser.next_packet().unwrap().is_none());
    }
}
