//! Music-bot audio pipeline — PURA-117 WS-2 ([PURA-119]).
//!
//! Pulls a source URL, decodes to 48 kHz s16le PCM, encodes Opus 20 ms frames,
//! paces them at wall-clock 20 ms cadence, and surfaces ICY metadata events.
//!
//! Lives in its own crate so [PURA-118] (WS-1, bot lifecycle in `crates/voice/`)
//! can take a path dependency without pulling the audio toolchain into the
//! lifecycle crate. The seam WS-1 plugs into is [`AudioPipeline`] — see the
//! crate-level docs in `docs/voice/audio-pipeline.md`.
//!
//! [PURA-118]: https://teamspeak-heaven/PURA/issues/PURA-118
//! [PURA-119]: https://teamspeak-heaven/PURA/issues/PURA-119

pub mod encoder;
pub mod icy;
pub mod pacer;
pub mod pipeline;
pub mod resolve;
pub mod source;
pub mod types;
pub mod volume;

pub use pipeline::AudioPipeline;
pub use types::{
    OpusFrame, PipelineConfig, PipelineError, PipelineEvent, SAMPLE_RATE_HZ,
    SAMPLES_PER_FRAME_MONO, frame_duration,
};
pub use volume::{MAX_GAIN, MIN_GAIN, UNITY_GAIN, VolumeHandle};
