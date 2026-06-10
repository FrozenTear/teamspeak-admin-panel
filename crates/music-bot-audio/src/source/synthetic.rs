//! Synthetic sine-wave PCM source. Used by the demo binary and the pacer test.

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

        // THE-981 (AR-9) — compute the phase in f64. The old
        // `omega_f32 * phase_samples as f32` lost integer precision past
        // 2²⁴ samples (~5.8 min at 48 kHz) and `sin()` of a large f32
        // argument quantises the phase audibly — an `infinite` soak tone
        // slowly turned harsh/detuned, which read exactly like a pipeline
        // bug during long manual probes. `phase_samples as f64` is exact to
        // 2⁵³ samples (~6 000 years) and the explicit `% TAU` keeps the
        // `sin` argument small where it is precise.
        let omega = std::f64::consts::TAU * self.hz as f64 / SAMPLE_RATE_HZ as f64;
        let amp = (self.amplitude * i16::MAX as f32) as i16 as f64;
        let mut i = 0;
        while i < to_write {
            let theta = (self.phase_samples as f64 * omega) % std::f64::consts::TAU;
            let s = (theta.sin() * amp) as i16;
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

    /// THE-981 (AR-9) — the tone must stay phase-accurate on long streams.
    /// The old `omega * phase_samples as f32` lost integer precision past
    /// 2²⁴ samples (~5.8 min at 48 kHz), quantising the phase audibly. Jump
    /// the accumulator to the 10-minute mark and check each emitted sample
    /// against an f64 reference oscillator.
    #[tokio::test]
    async fn tone_stays_accurate_past_f32_precision_horizon() {
        let hz = 440.0_f32;
        let mut src = SyntheticToneSource::new(hz, 0.5, 1, None);
        let ten_minutes: u64 = 48_000 * 600;
        src.phase_samples = ten_minutes;

        let mut buf = [0i16; 960];
        let n = src.read_samples(&mut buf).await.unwrap();
        assert_eq!(n, 960);

        let amp = (0.5 * i16::MAX as f32) as i16 as f64;
        let omega = 2.0 * std::f64::consts::PI * hz as f64 / 48_000.0;
        for (k, &s) in buf[..n].iter().enumerate() {
            let reference = ((ten_minutes + k as u64) as f64 * omega).sin() * amp;
            let err = (s as f64 - reference).abs();
            // f32 sin + rounding leaves a tiny residual; the old code was
            // off by hundreds-to-thousands of LSBs out here.
            assert!(
                err <= 24.0,
                "sample {k} at the 10-min mark off by {err:.1} LSB \
                 (got {s}, reference {reference:.1}) — phase quantisation",
            );
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
