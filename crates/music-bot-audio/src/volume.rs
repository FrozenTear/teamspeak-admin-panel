//! Shared output-gain control — the runtime-mutable volume seam (PURA-351).
//!
//! The music-bot dashboard's volume slider and the `!vol` chat command both
//! lower into a single linear-gain multiplier applied to PCM samples just
//! before the Opus encode. A [`VolumeHandle`] is an `Arc`-backed, lock-free
//! cell: the bot actor owns the canonical handle, hands a clone to each
//! [`AudioPipeline`](crate::AudioPipeline) it spawns, and `set`s it from the
//! REST / chat surfaces. Because every pipeline the bot spawns shares the
//! *same* handle, a volume change takes effect on the current track
//! immediately, is inherited by every later track, and survives a
//! reconnect — all without extra plumbing or a DB round-trip.
//!
//! ## Unit
//!
//! `gain` is a **linear amplitude multiplier**, not decibels: `1.0` = unity
//! (bit-exact pass-through), `0.0` = silence, `0.5` ≈ −6 dBFS. The dashboard
//! slider maps 0–100 % directly onto `0.0..=1.0`; the `!vol 0..100` chat
//! command divides by 100. Values are clamped to [`MIN_GAIN`]..=[`MAX_GAIN`];
//! `MAX_GAIN` leaves headroom for a modest boost of a quiet source while
//! bounding hard-clip distortion.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

/// Minimum gain — full silence.
pub const MIN_GAIN: f32 = 0.0;

/// Maximum gain. 2.0 (+6 dB) lets an operator lift a quiet source; the
/// dashboard slider caps the operator-facing range at 1.0 (100 %).
pub const MAX_GAIN: f32 = 2.0;

/// Unity gain — bit-exact pass-through, the default for a fresh pipeline.
pub const UNITY_GAIN: f32 = 1.0;

/// A shared, lock-free linear-gain cell. Cheap to clone (an `Arc` bump);
/// the pipeline worker reads it once per 20 ms frame.
#[derive(Clone, Debug)]
pub struct VolumeHandle(Arc<AtomicU32>);

impl VolumeHandle {
    /// Create a handle at `gain` (clamped to the valid range).
    pub fn new(gain: f32) -> Self {
        Self(Arc::new(AtomicU32::new(clamp_gain(gain).to_bits())))
    }

    /// Overwrite the gain. Clamped to [`MIN_GAIN`]..=[`MAX_GAIN`]; a `NaN`
    /// argument is rejected (the previous value is kept) so a malformed
    /// wire value can never silence or blast a live stream.
    pub fn set(&self, gain: f32) {
        if gain.is_nan() {
            return;
        }
        self.0.store(clamp_gain(gain).to_bits(), Ordering::Relaxed);
    }

    /// Current gain.
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
}

impl Default for VolumeHandle {
    /// Unity gain — a pipeline spawned without an explicit volume plays at
    /// the source's native level.
    fn default() -> Self {
        Self::new(UNITY_GAIN)
    }
}

/// Clamp an arbitrary `f32` into the valid gain range. `NaN` callers are
/// filtered upstream by [`VolumeHandle::set`]; a `NaN` reaching here folds
/// to [`UNITY_GAIN`] defensively.
fn clamp_gain(gain: f32) -> f32 {
    if gain.is_nan() {
        return UNITY_GAIN;
    }
    gain.clamp(MIN_GAIN, MAX_GAIN)
}

/// Apply `gain` to one PCM frame in place. Unity gain (within
/// `f32::EPSILON`) is a no-op fast path — the common case while the
/// operator leaves the slider alone. Otherwise each `i16` sample is scaled
/// and clamped to the `i16` range, so a boost above unity hard-limits
/// instead of wrapping into noise.
pub(crate) fn apply_gain(pcm: &mut [i16], gain: f32) {
    if (gain - UNITY_GAIN).abs() <= f32::EPSILON {
        return;
    }
    for sample in pcm.iter_mut() {
        let scaled = *sample as f32 * gain;
        *sample = scaled.clamp(i16::MIN as f32, i16::MAX as f32) as i16;
    }
}

/// THE-983 (AR-3) — apply a gain *ramp* to one interleaved PCM frame in
/// place, lerping from `from` to `to` across the frame's channel strides.
/// A flat per-frame multiplier puts a step discontinuity on the waveform
/// at every `!vol` / slider move — audible as a click. Spreading the change
/// across the 20 ms frame keeps it inaudible. All `channels` samples of one
/// stride get the same gain so the ramp never skews the stereo image; the
/// last stride lands exactly on `to`, so the next frame's flat
/// [`apply_gain`] continues seamlessly.
pub(crate) fn apply_gain_ramp(pcm: &mut [i16], from: f32, to: f32, channels: u8) {
    let stride = channels.max(1) as usize;
    let strides = pcm.len() / stride;
    if (from - to).abs() <= f32::EPSILON || strides < 2 {
        return apply_gain(pcm, to);
    }
    let last = strides - 1;
    let step = (to - from) / last as f32;
    for (i, frame) in pcm.chunks_mut(stride).enumerate() {
        // Pin the last stride to `to` exactly — `from + step * last` can
        // round 1 ulp off in f32 and put a residual step on the frame seam.
        let gain = if i == last {
            to
        } else {
            from + step * i as f32
        };
        for sample in frame {
            let scaled = *sample as f32 * gain;
            *sample = scaled.clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_unity() {
        assert_eq!(VolumeHandle::default().get(), UNITY_GAIN);
    }

    #[test]
    fn set_get_roundtrips() {
        let v = VolumeHandle::default();
        v.set(0.42);
        assert_eq!(v.get(), 0.42);
    }

    #[test]
    fn clones_share_one_cell() {
        let a = VolumeHandle::default();
        let b = a.clone();
        a.set(0.25);
        assert_eq!(b.get(), 0.25, "clone observes the writer's update");
    }

    #[test]
    fn set_clamps_out_of_range() {
        let v = VolumeHandle::default();
        v.set(-3.0);
        assert_eq!(v.get(), MIN_GAIN, "negative gain clamps to silence");
        v.set(99.0);
        assert_eq!(v.get(), MAX_GAIN, "excessive gain clamps to the ceiling");
    }

    #[test]
    fn set_rejects_nan() {
        let v = VolumeHandle::new(0.6);
        v.set(f32::NAN);
        assert_eq!(v.get(), 0.6, "NaN leaves the previous value untouched");
    }

    #[test]
    fn unity_gain_is_a_no_op() {
        let mut pcm = [-12345i16, 0, 12345, i16::MIN, i16::MAX];
        let original = pcm;
        apply_gain(&mut pcm, UNITY_GAIN);
        assert_eq!(pcm, original, "unity gain leaves PCM bit-exact");
    }

    #[test]
    fn half_gain_halves_samples() {
        let mut pcm = [-1000i16, 2000, 4000];
        apply_gain(&mut pcm, 0.5);
        assert_eq!(pcm, [-500, 1000, 2000]);
    }

    #[test]
    fn zero_gain_silences() {
        let mut pcm = [i16::MIN, -1, 1, i16::MAX];
        apply_gain(&mut pcm, 0.0);
        assert_eq!(pcm, [0, 0, 0, 0]);
    }

    #[test]
    fn ramp_is_monotonic_and_lands_on_target() {
        // Constant-amplitude mono frame: the output must trace the ramp.
        let mut pcm = [10_000i16; 100];
        apply_gain_ramp(&mut pcm, 1.0, 0.0, 1);
        assert_eq!(pcm[0], 10_000, "first sample starts at `from`");
        assert_eq!(pcm[99], 0, "last sample lands exactly on `to`");
        for w in pcm.windows(2) {
            assert!(w[1] <= w[0], "fade-down must be monotonic: {w:?}");
        }
        // No flat-multiplier step: adjacent samples differ by ~from-to/99.
        let max_step = pcm.windows(2).map(|w| (w[0] - w[1]).abs()).max().unwrap();
        assert!(max_step <= 102, "step discontinuity in ramp: {max_step}");
    }

    #[test]
    fn ramp_applies_equal_gain_per_stereo_stride() {
        // L = 8000, R = -4000 interleaved; the L/R ratio must be preserved
        // at every stride — a per-sample (not per-stride) ramp would skew
        // the stereo image.
        let mut pcm = [8000i16, -4000].repeat(48);
        apply_gain_ramp(&mut pcm, 1.0, 0.5, 2);
        for pair in pcm.chunks(2) {
            // f32→i16 truncation costs <1 LSB per channel, so the exact
            // 2:1 ratio holds to within 2 LSB at every stride.
            let skew = (pair[0] as i32 + 2 * pair[1] as i32).abs();
            assert!(skew <= 2, "L/R ratio skewed by {skew}: {pair:?}");
        }
        assert_eq!(&pcm[..2], &[8000, -4000], "first stride at `from`");
        assert_eq!(&pcm[94..], &[4000, -2000], "last stride at `to`");
    }

    #[test]
    fn ramp_with_equal_endpoints_is_flat_apply() {
        let mut ramped = [-1000i16, 2000, 4000];
        let mut flat = ramped;
        apply_gain_ramp(&mut ramped, 0.5, 0.5, 1);
        apply_gain(&mut flat, 0.5);
        assert_eq!(ramped, flat, "no-change frame degrades to flat gain");
    }

    #[test]
    fn boost_hard_limits_instead_of_wrapping() {
        let mut pcm = [20000i16, -20000];
        apply_gain(&mut pcm, 2.0);
        assert_eq!(
            pcm,
            [i16::MAX, i16::MIN],
            "a boost past full-scale clamps, never wraps",
        );
    }
}
