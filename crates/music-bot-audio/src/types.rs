//! Public types — the seam WS-1 (bot lifecycle) consumes.

use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Opus voice frames are pinned to 48 kHz per RFC 7587 / TS6 §19.10.
pub const SAMPLE_RATE_HZ: u32 = 48_000;

/// 20 ms frame, mono. Matches the rhythm `crates/ts6-voice-prototype` settled
/// on for live TS6 transmission.
pub const SAMPLES_PER_FRAME_MONO: usize = (SAMPLE_RATE_HZ as usize / 1000) * 20;

/// Wall-clock duration of a single Opus frame.
pub const fn frame_duration() -> Duration {
    Duration::from_millis(20)
}

/// One paced Opus frame ready for `tsclientlib`'s `OutAudio::new(AudioData::C2S {…})`.
#[derive(Debug, Clone)]
pub struct OpusFrame {
    /// Raw Opus packet bytes (no TS6 voice header — WS-1 wraps it).
    pub bytes: Vec<u8>,
    /// Monotonic frame index, starting at 0.
    pub index: u64,
    /// `Instant` the frame was *intended* to play at, derived from the pipeline
    /// start instant + `index * 20 ms`. Drift-free: NOT `Instant::now()` at
    /// emit time. WS-1 sleeps the difference between this and the live clock
    /// before pushing the frame on the wire.
    pub scheduled_at: Instant,
    /// PCM channel count the encoder was configured with (1 or 2). TS6 voice
    /// is conventionally mono; stereo is supported for archival / WS-7 paths.
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
    /// Frame channel buffer depth. WS-1 should drain at 20 ms cadence; a
    /// shallow buffer is fine and keeps us from pre-rolling a big backlog.
    pub frame_buffer: usize,
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
