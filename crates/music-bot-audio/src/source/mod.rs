//! Audio sources — anything that produces 48 kHz s16le interleaved PCM.

use async_trait::async_trait;

use crate::types::PipelineEvent;

pub mod ffmpeg;
pub mod icy;
pub mod synthetic;
pub mod url;

pub use ffmpeg::FfmpegSource;
pub use icy::IcyRadioSource;
pub use synthetic::SyntheticToneSource;
pub use url::YtDlpSource;

/// Anything the pipeline can pull s16le PCM samples out of.
///
/// Cancel-safety: implementations MUST NOT corrupt their own state if the
/// `read_samples` future is dropped mid-await — the pipeline aborts source
/// reads when its consumer disconnects.
#[async_trait]
pub trait PcmSource: Send {
    /// Fill up to `buf.len()` interleaved samples; returns how many were
    /// written. `0` means clean EOF and the pipeline shuts down.
    async fn read_samples(&mut self, buf: &mut [i16]) -> std::io::Result<usize>;

    /// Drain any pending out-of-band events without blocking. The pipeline
    /// calls this between `read_samples` rounds and forwards the events on
    /// the broadcast channel.
    fn try_drain_events(&mut self) -> Vec<PipelineEvent> {
        Vec::new()
    }
}

/// Source factory request — what the pipeline owner asks for.
#[derive(Debug, Clone)]
pub enum AudioSourceSpec {
    /// In-process sine-wave generator. Useful for tests and the demo binary.
    SyntheticTone {
        hz: f32,
        amplitude: f32,
        /// Optional duration; `None` = forever.
        duration_ms: Option<u64>,
    },
    /// Decode an arbitrary file or URL via `ffmpeg` directly. The path is
    /// passed as `-i <path>`; works for local files, HTTP URLs that ffmpeg
    /// understands, etc. No yt-dlp involvement.
    Ffmpeg { input: String },
    /// `yt-dlp -f bestaudio -o - <url>` piped into ffmpeg. The right shape
    /// for YouTube / SoundCloud / arbitrary "play this URL" inputs.
    YtDlp { url: String },
    /// Direct ICY HTTP fetch (Shoutcast / Icecast). Surfaces `StreamTitle`
    /// changes as `PipelineEvent::NowPlaying`.
    IcyRadio { url: String },
}
