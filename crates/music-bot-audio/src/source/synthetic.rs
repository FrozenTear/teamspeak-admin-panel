//! Synthetic sine-wave PCM source. Used by the demo binary and the pacer test.

use std::f32::consts::PI;

use async_trait::async_trait;

use super::PcmSource;
use crate::types::SAMPLE_RATE_HZ;

pub struct SyntheticToneSource {
    hz: f32,
    amplitude: f32,
    channels: u8,
    /// Phase accumulator in samples (mono-time).
    phase_samples: u64,
    /// Optional bound; `None` = infinite stream.
    samples_remaining: Option<u64>,
}

impl SyntheticToneSource {
    pub fn new(hz: f32, amplitude: f32, channels: u8, duration_ms: Option<u64>) -> Self {
        let samples_remaining = duration_ms.map(|ms| {
            // Per-channel sample count = SR * ms / 1000.
            // The interleaved buffer carries `channels * per_channel` samples.
            (SAMPLE_RATE_HZ as u64 * ms / 1000) * channels as u64
        });
        Self {
            hz,
            amplitude: amplitude.clamp(0.0, 1.0),
            channels,
            phase_samples: 0,
            samples_remaining,
        }
    }
}

#[async_trait]
impl PcmSource for SyntheticToneSource {
    async fn read_samples(&mut self, buf: &mut [i16]) -> std::io::Result<usize> {
        let mut available = buf.len();
        if let Some(remaining) = self.samples_remaining {
            if remaining == 0 {
                return Ok(0);
            }
            available = available.min(remaining as usize);
        }
        // Round down to a whole frame stride so callers always see complete
        // L/R pairs in stereo. Mono is a no-op.
        let stride = self.channels as usize;
        let to_write = (available / stride) * stride;
        if to_write == 0 {
            return Ok(0);
        }

        let omega = 2.0 * PI * self.hz / SAMPLE_RATE_HZ as f32;
        let amp_i16 = (self.amplitude * i16::MAX as f32) as i16;
        let mut i = 0;
        while i < to_write {
            let theta = omega * self.phase_samples as f32;
            let s = (theta.sin() * amp_i16 as f32) as i16;
            for ch in 0..stride {
                buf[i + ch] = s;
            }
            i += stride;
            self.phase_samples = self.phase_samples.wrapping_add(1);
        }
        if let Some(r) = self.samples_remaining.as_mut() {
            *r -= to_write as u64;
        }
        Ok(to_write)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mono_emits_requested_window() {
        let mut src = SyntheticToneSource::new(440.0, 0.5, 1, None);
        let mut buf = [0i16; 960];
        let n = src.read_samples(&mut buf).await.unwrap();
        assert_eq!(n, 960);
        // First sample is sin(0)=0, well-defined.
        assert_eq!(buf[0], 0);
        // Tone should produce a non-zero next sample.
        assert!(buf[1] != 0);
    }

    #[tokio::test]
    async fn stereo_writes_paired_samples() {
        let mut src = SyntheticToneSource::new(440.0, 0.5, 2, None);
        let mut buf = [0i16; 1920];
        let n = src.read_samples(&mut buf).await.unwrap();
        assert_eq!(n, 1920);
        // L and R of the same instant should match (we generate a single tone).
        for i in (0..n).step_by(2) {
            assert_eq!(buf[i], buf[i + 1]);
        }
    }

    #[tokio::test]
    async fn duration_eof() {
        // 40 ms @ 48kHz mono = 1920 samples.
        let mut src = SyntheticToneSource::new(440.0, 0.5, 1, Some(40));
        let mut buf = [0i16; 4096];
        let n = src.read_samples(&mut buf).await.unwrap();
        assert_eq!(n, 1920);
        let n2 = src.read_samples(&mut buf).await.unwrap();
        assert_eq!(n2, 0, "second read sees EOF");
    }
}
