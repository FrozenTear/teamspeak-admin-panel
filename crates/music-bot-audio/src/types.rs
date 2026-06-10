//! Public types — the seam WS-1 (bot lifecycle) consumes.

use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Opus voice frames are pinned to 48 kHz per RFC 7587 / TS6 §19.10.
pub const SAMPLE_RATE_HZ: u32 = 48_000;

/// 20 ms frame, mono. Matches the rhythm `crates/ts6-voice-prototype` settled
/// on for live TS6 transmission.
pub const SAMPLES_PER_FRAME_MONO: usize = (SAMPLE_RATE_HZ as usize / 1000) * 20;

/// THE-986 — heap bytes one mono [`PcmFrame`] holds (960 × `i16` ≈ 1.9 kB;
/// double for stereo). The frame channel buffers *PCM* now, not encoded
/// Opus: the music bot's 250-frame channel holds ≈ 480 kB of PCM per
/// playing bot (mono; ≈ 960 kB stereo) versus ~75 kB encoded — the
/// deliberate price for moving gain + encode to the consumer side of the
/// buffer, which bounds `!vol` latency to ≤ 1–2 frames instead of the full
/// channel backlog (≈ 5 s on fast sources).
pub const PCM_FRAME_BYTES_MONO: usize = SAMPLES_PER_FRAME_MONO * std::mem::size_of::<i16>();

/// Wall-clock duration of a single Opus frame.
pub const fn frame_duration() -> Duration {
    Duration::from_millis(20)
}

/// One paced 20 ms PCM frame, ready for the consumer-side gain + Opus
/// encode.
///
/// THE-986 — the pipeline used to emit *encoded* `OpusFrame`s, with gain
/// applied at encode time in the worker; a `!vol` move then sat behind the
/// in-flight frames in the channel (≈ 5 s on fast sources) before becoming
/// audible. Emitting PCM and letting the consumer apply gain + encode at
/// dequeue makes volume latency uniform across source types.
#[derive(Debug, Clone)]
pub struct PcmFrame {
    /// Interleaved 48 kHz s16le PCM — exactly one 20 ms frame
    /// (`SAMPLES_PER_FRAME_MONO * channels` samples; an EOF-short tail is
    /// silence-padded by the worker, so the consumer's encoder always sees
    /// a full frame).
    pub samples: Vec<i16>,
    /// Monotonic frame index, starting at 0.
    pub index: u64,
    /// `Instant` the frame was *intended* to play at, derived from the pipeline
    /// start instant + `index * 20 ms`. Drift-free: NOT `Instant::now()` at
    /// emit time. WS-1 sleeps the difference between this and the live clock
    /// before pushing the frame on the wire.
    pub scheduled_at: Instant,
    /// PCM channel count (1 or 2). TS6 voice is conventionally mono; stereo
    /// is supported for archival / WS-7 paths.
    pub channels: u8,
}

/// Out-of-band events the pipeline raises. WS-1 maps these onto the supervisor
/// `BotEvent` variants (`NowPlaying`, `Error`, `EndOfStream`).
#[derive(Debug, Clone)]
pub enum PipelineEvent {
    /// ICY `StreamTitle` change observed on a radio source. WS-1 forwards this
    /// to chat / REST / FE-PAGES as `BotEvent::NowPlaying`.
    NowPlaying { title: String, source: String },
    /// The source closed cleanly (track ended, stream EOF). The frame channel
    /// will close shortly after.
    EndOfStream,
    /// Non-fatal subsystem error. Pipeline keeps running where possible.
    Warning(String),
}

#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Opus channel layout. 1 = mono (TS6 voice default), 2 = stereo.
    pub channels: u8,
    /// Opus encoder bitrate (bits/sec). `None` = libopus default for the
    /// chosen application + sample rate. 64_000 is a reasonable music default.
    pub bitrate_bps: Option<i32>,
    /// `audiopus::Application::Audio` for music, `Voip` for speech. Music-bot
    /// defaults to `Audio`.
    pub application: OpusApplication,
    /// Frame channel buffer depth — the consumer's stall runway. WS-1 drains
    /// at a fixed 20 ms cadence (PURA-323 wall-clock pacing), so a producer
    /// stall longer than `frame_buffer * 20 ms` empties the channel and gaps
    /// the wire — audible crackle (PURA-329). Size it for the worst expected
    /// network / ffmpeg hiccup: the music bot uses 100 frames (2 s).
    /// THE-986 — each buffered frame is PCM ([`PCM_FRAME_BYTES_MONO`] per
    /// channel), so depth now costs ~25× more memory than the encoded-Opus
    /// era; see the constant for the per-bot figure.
    pub frame_buffer: usize,
    /// Pre-buffer watermark (PURA-329). The pipeline worker holds the first
    /// `prebuffer_frames` encoded frames before it anchors the pacer and
    /// forwards anything, so the consumer starts draining against an
    /// already-filled channel and a transient producer stall during start-up
    /// cannot immediately underrun. `0` = no pre-buffer (pacer anchors on the
    /// first frame — legacy behaviour). Should be `<= frame_buffer`; tracks
    /// shorter than the watermark flush early at EOF.
    pub prebuffer_frames: usize,
    /// Event broadcast capacity. Subscribers that lag past this are dropped
    /// — events are advisory.
    pub event_buffer: usize,
    /// PURA-223 — resolved Netscape `cookies.txt` path for yt-dlp. `None`
    /// means no cookies (anonymous). Resolved by the caller at play-time
    /// from `app_setting:yt_cookie_path` (DB) or `YT_COOKIE_FILE` env so
    /// a UI-uploaded cookie is effective without a manager restart.
    pub yt_cookie_file: Option<PathBuf>,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            channels: 1,
            bitrate_bps: Some(64_000),
            application: OpusApplication::Audio,
            frame_buffer: 8,
            prebuffer_frames: 0,
            event_buffer: 32,
            yt_cookie_file: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum OpusApplication {
    Voip,
    Audio,
    LowDelay,
}

impl From<OpusApplication> for audiopus::Application {
    fn from(value: OpusApplication) -> Self {
        match value {
            OpusApplication::Voip => audiopus::Application::Voip,
            OpusApplication::Audio => audiopus::Application::Audio,
            OpusApplication::LowDelay => audiopus::Application::LowDelay,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("opus encoder init failed: {0}")]
    EncoderInit(String),
    #[error("opus encode failed: {0}")]
    Encode(String),
    #[error("audio source error: {0}")]
    Source(String),
    #[error("invalid channel count {0} (expected 1 or 2)")]
    InvalidChannels(u8),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}
