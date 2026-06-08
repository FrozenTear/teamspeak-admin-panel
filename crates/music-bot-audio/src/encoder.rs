//! Opus 20 ms encoder — one frame in, one Opus packet out.

use audiopus::{Bitrate, Channels, SampleRate, coder::Encoder};
use bytes::{Bytes, BytesMut};

use crate::types::{PipelineConfig, PipelineError, SAMPLE_RATE_HZ, SAMPLES_PER_FRAME_MONO};

/// Opus packet bytes never exceed 4 kB at the bitrates we use; matches the
/// `MAX_OPUS_FRAME` constant in `crates/ts6-voice-prototype`.
pub const MAX_OPUS_PACKET_BYTES: usize = 4_000;

/// THE-922 — scratch chunk size for the encoder's reused `BytesMut`. Each
/// chunk feeds many `encode_frame` calls: an Opus packet is typically
/// 100–300 bytes (64 kbps stereo at 20 ms), so a 256 kB chunk amortises the
/// `BytesMut` growth/shared-promote across ~hundreds of frames, dropping
/// the encoder's allocation count from per-frame (the `to_vec` baseline
/// measured on THE-900) to a rare grow on chunk exhaustion. A smaller
/// chunk would put us back into the per-frame realloc regime because
/// `split_to(n).freeze()` keeps the carved-off bytes alive until the
/// consumer drains them, so the `BytesMut`'s reclaim path can't run.
const SCRATCH_CHUNK_BYTES: usize = MAX_OPUS_PACKET_BYTES * 64;

pub struct OpusFrameEncoder {
    inner: Encoder,
    channels: u8,
    samples_per_frame: usize,
    /// Reused scratch buffer for the encode. The buffer is sized to hold
    /// many Opus packets back-to-back; each `encode_frame` writes one packet
    /// into the live `MAX_OPUS_PACKET_BYTES`-long window at the head of
    /// the buffer, carves off the encoded prefix as a `Bytes`, then resets
    /// the window to length `MAX_OPUS_PACKET_BYTES` (no realloc as long as
    /// the chunk still has spare capacity behind the carved-off frames).
    /// When the chunk is exhausted, `BytesMut::resize` falls through to a
    /// single fresh allocation — the steady-state amortised cost.
    scratch: BytesMut,
}

impl OpusFrameEncoder {
    pub fn new(cfg: &PipelineConfig) -> Result<Self, PipelineError> {
        let channels = match cfg.channels {
            1 => Channels::Mono,
            2 => Channels::Stereo,
            other => return Err(PipelineError::InvalidChannels(other)),
        };
        let mut inner = Encoder::new(SampleRate::Hz48000, channels, cfg.application.into())
            .map_err(|e| PipelineError::EncoderInit(e.to_string()))?;
        if let Some(bps) = cfg.bitrate_bps {
            inner
                .set_bitrate(Bitrate::BitsPerSecond(bps))
                .map_err(|e| PipelineError::EncoderInit(format!("set_bitrate({bps}): {e}")))?;
        }
        let mut scratch = BytesMut::with_capacity(SCRATCH_CHUNK_BYTES);
        scratch.resize(MAX_OPUS_PACKET_BYTES, 0);
        Ok(Self {
            inner,
            channels: cfg.channels,
            samples_per_frame: SAMPLES_PER_FRAME_MONO * cfg.channels as usize,
            scratch,
        })
    }

    /// Number of `i16` PCM samples one 20 ms frame consumes. For stereo this
    /// includes both channels (interleaved L,R,L,R,…).
    pub fn samples_per_frame(&self) -> usize {
        self.samples_per_frame
    }

    pub fn channels(&self) -> u8 {
        self.channels
    }

    pub fn sample_rate(&self) -> u32 {
        SAMPLE_RATE_HZ
    }

    /// Encode exactly one 20 ms frame. `pcm` MUST be `samples_per_frame()`
    /// long; shorter frames are padded with silence by the caller (see
    /// `Pipeline::pump_one`).
    ///
    /// Returns the encoded Opus packet as a `Bytes` slice carved from the
    /// reused `scratch` buffer. The remainder of `scratch` is re-grown back
    /// to `MAX_OPUS_PACKET_BYTES` so the next call writes into stable
    /// capacity (one amortized grow at steady state, not per frame).
    pub fn encode_frame(&mut self, pcm: &[i16]) -> Result<Bytes, PipelineError> {
        if pcm.len() != self.samples_per_frame {
            return Err(PipelineError::Encode(format!(
                "expected {} pcm samples, got {}",
                self.samples_per_frame,
                pcm.len()
            )));
        }
        let n = self
            .inner
            .encode(pcm, self.scratch.as_mut())
            .map_err(|e| PipelineError::Encode(e.to_string()))?;
        let frame = self.scratch.split_to(n).freeze();
        self.scratch.resize(MAX_OPUS_PACKET_BYTES, 0);
        Ok(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::OpusApplication;

    fn cfg(channels: u8) -> PipelineConfig {
        PipelineConfig {
            channels,
            bitrate_bps: Some(64_000),
            application: OpusApplication::Audio,
            frame_buffer: 4,
            prebuffer_frames: 0,
            event_buffer: 4,
            yt_cookie_file: None,
        }
    }

    #[test]
    fn mono_silence_frame_encodes() {
        let mut enc = OpusFrameEncoder::new(&cfg(1)).expect("encoder");
        let pcm = vec![0i16; enc.samples_per_frame()];
        let pkt = enc.encode_frame(&pcm).expect("encode");
        assert!(!pkt.is_empty(), "silence still emits a non-empty packet");
        assert!(pkt.len() < MAX_OPUS_PACKET_BYTES);
    }

    #[test]
    fn stereo_frame_doubles_pcm_window() {
        let mut enc = OpusFrameEncoder::new(&cfg(2)).expect("encoder");
        assert_eq!(enc.samples_per_frame(), SAMPLES_PER_FRAME_MONO * 2);
        let pcm = vec![0i16; enc.samples_per_frame()];
        let pkt = enc.encode_frame(&pcm).expect("encode");
        assert!(!pkt.is_empty());
    }

    #[test]
    fn wrong_pcm_len_errors() {
        let mut enc = OpusFrameEncoder::new(&cfg(1)).expect("encoder");
        let err = enc
            .encode_frame(&[0i16; 100])
            .expect_err("short pcm rejected");
        assert!(matches!(err, PipelineError::Encode(_)));
    }
}
