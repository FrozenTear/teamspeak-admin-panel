//! Opus 20 ms encoder — one frame in, one Opus packet out.

use audiopus::{Bitrate, Channels, SampleRate, coder::Encoder};

use crate::types::{PipelineConfig, PipelineError, SAMPLE_RATE_HZ, SAMPLES_PER_FRAME_MONO};

/// Opus packet bytes never exceed 4 kB at the bitrates we use; matches the
/// `MAX_OPUS_FRAME` constant in `crates/ts6-voice-prototype`.
pub const MAX_OPUS_PACKET_BYTES: usize = 4_000;

pub struct OpusFrameEncoder {
    inner: Encoder,
    channels: u8,
    samples_per_frame: usize,
    scratch: [u8; MAX_OPUS_PACKET_BYTES],
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
        Ok(Self {
            inner,
            channels: cfg.channels,
            samples_per_frame: SAMPLES_PER_FRAME_MONO * cfg.channels as usize,
            scratch: [0u8; MAX_OPUS_PACKET_BYTES],
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
    pub fn encode_frame(&mut self, pcm: &[i16]) -> Result<Vec<u8>, PipelineError> {
        if pcm.len() != self.samples_per_frame {
            return Err(PipelineError::Encode(format!(
                "expected {} pcm samples, got {}",
                self.samples_per_frame,
                pcm.len()
            )));
        }
        let n = self
            .inner
            .encode(pcm, &mut self.scratch)
            .map_err(|e| PipelineError::Encode(e.to_string()))?;
        Ok(self.scratch[..n].to_vec())
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
