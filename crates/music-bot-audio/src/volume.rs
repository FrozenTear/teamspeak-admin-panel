//! Shared output-gain control — the runtime-mutable volume seam (PURA-351).
//!
//! The music-bot dashboard's volume slider and the `!vol` chat command both
//! lower into a single linear-gain multiplier applied to PCM samples just
//! before the Opus encode — on the *consumer* side of the frame channel
//! (THE-986, via [`GainStage`]). A [`VolumeHandle`] is an `Arc`-backed,
//! lock-free cell: the bot actor owns the canonical handle, hands a clone to
//! each play's consumer-side gain stage, and `set`s it from the REST / chat
//! surfaces. Because every play shares the *same* handle, a volume change
//! takes effect on the current track within ≤ 1–2 frames, is inherited by
//! every later track, and survives a reconnect — all without extra plumbing
//! or a DB round-trip.
//!
//! ## Unit
//!
//! `gain` is a **linear amplitude multiplier**, not decibels: `1.0` = unity
//! (bit-exact pass-through), `0.0` = silence, `0.5` ≈ −6 dBFS. The dashboard
//! slider maps 0–100 % directly onto `0.0..=1.0`; the `!vol 0..100` chat
//! command divides by 100. Values are clamped to [`MIN_GAIN`]..=[`MAX_GAIN`];
//! `MAX_GAIN` leaves headroom for a modest boost of a quiet source; peaks
//! that a boost pushes past full scale are rounded off by the THE-987
//! soft-knee saturator instead of hard-clipping.

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

/// THE-986 — per-play gain stage, applied by the *consumer* at frame
/// dequeue. Tracks the gain the previous frame ended on and applies either
/// the flat (unity-fast-path) gain or the THE-983 (AR-3) click-free ramp on
/// the frame where the gain changes.
///
/// This used to live in the pipeline worker at encode time, which put every
/// `!vol` / slider move behind the frame channel's in-flight backlog — ≈ 5 s
/// on fast (yt-dlp / file) sources whose channel runs full, near-instant on
/// radio. At the dequeue side the move is audible within ≤ 1–2 frames,
/// uniform across source types. The THE-987 (AR-6) soft-knee saturator
/// lives in this stage too, inside the gain applies.
pub struct GainStage {
    handle: VolumeHandle,
    /// THE-983 (AR-3) — the gain the previous frame ended on; a change is
    /// lerped from this to the new target across the change frame instead
    /// of stepping (a step is an audible click).
    prev_gain: f32,
}

impl GainStage {
    /// Build a stage reading from `handle`. The first frame starts from the
    /// handle's *current* gain, so a volume set before playback applies
    /// flat rather than ramping from unity.
    pub fn new(handle: VolumeHandle) -> Self {
        let prev_gain = handle.get();
        Self { handle, prev_gain }
    }

    /// Apply the operator's current gain to one interleaved PCM frame in
    /// place. Read once per frame so a mid-track move is picked up on the
    /// next 20 ms boundary; steady-state frames take the flat apply, the
    /// change frame is ramped.
    pub fn apply(&mut self, pcm: &mut [i16], channels: u8) {
        let target = self.handle.get();
        if (target - self.prev_gain).abs() > f32::EPSILON {
            apply_gain_ramp(pcm, self.prev_gain, target, channels);
        } else {
            apply_gain(pcm, target);
        }
        self.prev_gain = target;
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

/// THE-987 (AR-6) — soft-knee threshold as a fraction of full scale:
/// −1 dBFS (`10^(-1/20)`). Scaled samples below this stay exactly linear;
/// above it the saturator curve takes over.
const KNEE: f32 = 0.891_250_9;

/// Full scale for the saturator's normalized domain. `i16::MAX` for *both*
/// polarities keeps the curve odd-symmetric — the i16 range itself is not.
const FULL_SCALE: f32 = i16::MAX as f32;

/// THE-987 (AR-6) — scale one sample by `gain`, soft-kneeing the result
/// when the gain is a boost. At `gain <= 1.0` this is the original
/// scale-and-clamp, bit-exact with the pre-THE-987 path (the clamp never
/// engages below unity; it guards the float→int cast). At `gain > 1.0` a
/// plain clamp flat-tops every peak a boost pushes past full scale —
/// audible as crunchy crackle on loud passages of mastered (already
/// near-0 dBFS) music — so the scaled value passes [`soft_knee`] instead.
#[inline]
fn scale_sample(sample: i16, gain: f32) -> i16 {
    let scaled = sample as f32 * gain;
    if gain > UNITY_GAIN {
        soft_knee(scaled)
    } else {
        scaled.clamp(i16::MIN as f32, i16::MAX as f32) as i16
    }
}

/// Saturate one already-scaled sample value with a −1 dBFS soft knee.
/// Below the knee the curve is the identity, so quiet-to-moderate material
/// under a boost stays sample-exact linear. Above it, the overshoot maps
/// through a scaled `atan` — value- and slope-continuous at the knee
/// (`atan'(0) = 1`), strictly increasing, and asymptotic to full scale —
/// so peaks compress progressively instead of flat-topping. Odd-symmetric
/// by construction (the curve is applied to the magnitude, sign restored).
#[inline]
fn soft_knee(scaled: f32) -> i16 {
    use std::f32::consts::{FRAC_2_PI, FRAC_PI_2};
    let mag = scaled.abs() / FULL_SCALE;
    if mag <= KNEE {
        // Identity region: same float→int cast as the linear path.
        return scaled as i16;
    }
    let over = (mag - KNEE) / (1.0 - KNEE);
    let y = KNEE + (1.0 - KNEE) * FRAC_2_PI * (FRAC_PI_2 * over).atan();
    (y * FULL_SCALE).copysign(scaled) as i16
}

/// Apply `gain` to one PCM frame in place. Unity gain (within
/// `f32::EPSILON`) is a no-op fast path — the common case while the
/// operator leaves the slider alone. Otherwise each `i16` sample goes
/// through [`scale_sample`]: linear below unity, soft-kneed above it.
pub(crate) fn apply_gain(pcm: &mut [i16], gain: f32) {
    if (gain - UNITY_GAIN).abs() <= f32::EPSILON {
        return;
    }
    for sample in pcm.iter_mut() {
        *sample = scale_sample(*sample, gain);
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
        // Composes with THE-987: each stride's lerped gain feeds the same
        // [`scale_sample`] as the flat apply, so a ramp through a boost
        // soft-knees exactly like the steady-state frames around it.
        for sample in frame {
            *sample = scale_sample(*sample, gain);
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

    /// THE-986 — steady-state frames take the flat apply: two frames at an
    /// unchanged gain come out identical to a plain [`apply_gain`].
    #[test]
    fn gain_stage_steady_state_is_flat() {
        let handle = VolumeHandle::new(0.5);
        let mut stage = GainStage::new(handle);
        for _ in 0..2 {
            let mut staged = [-1000i16, 2000, 4000];
            let mut flat = staged;
            stage.apply(&mut staged, 1);
            apply_gain(&mut flat, 0.5);
            assert_eq!(staged, flat, "steady-state frame is the flat apply");
        }
    }

    /// THE-986 — the frame where the gain changes is ramped from the
    /// previous frame's gain (THE-983 AR-3), and the following frame is
    /// flat at the new target.
    #[test]
    fn gain_stage_ramps_on_change_then_settles() {
        let handle = VolumeHandle::default();
        let mut stage = GainStage::new(handle.clone());
        // Unity frame — untouched.
        let mut frame = [10_000i16; 100];
        stage.apply(&mut frame, 1);
        assert_eq!(frame, [10_000i16; 100], "unity steady state is bit-exact");
        // Cut to silence: the change frame ramps 1.0 → 0.0 across its
        // strides instead of stepping.
        handle.set(0.0);
        let mut change = [10_000i16; 100];
        stage.apply(&mut change, 1);
        assert_eq!(change[0], 10_000, "ramp starts at the previous gain");
        assert_eq!(change[99], 0, "ramp lands exactly on the target");
        assert!(
            change.windows(2).all(|w| w[1] <= w[0]),
            "fade-down must be monotonic",
        );
        // Next frame is flat at the new target.
        let mut settled = [10_000i16; 100];
        stage.apply(&mut settled, 1);
        assert_eq!(settled, [0i16; 100], "post-change frame is flat at target");
    }

    /// THE-987 (AR-6) — under a boost, scaled values that stay below the
    /// −1 dBFS knee pass through sample-exact linear: the saturator must
    /// not colour material the boost doesn't actually push near full scale.
    #[test]
    fn boost_below_knee_is_sample_exact_linear() {
        // 2.0 × 14_000 = 28_000 < knee (≈ 29_204), for every sample here.
        let mut pcm = [-14_000i16, -7_321, 0, 1, 9_999, 14_000];
        apply_gain(&mut pcm, 2.0);
        assert_eq!(
            pcm,
            [-28_000, -14_642, 0, 2, 19_998, 28_000],
            "below-knee boost must be exactly 2×",
        );
    }

    /// THE-987 (AR-6) — the saturator output is strictly bounded: even the
    /// most negative sample at maximum boost stays short of full scale, so
    /// no flat-top (and certainly no wrap) is possible.
    #[test]
    fn boost_output_strictly_bounded() {
        let mut pcm = [i16::MIN, -30_000, 30_000, i16::MAX];
        apply_gain(&mut pcm, MAX_GAIN);
        for s in pcm {
            assert!(
                (s as i32).abs() < i16::MAX as i32,
                "saturated sample {s} reached full scale",
            );
        }
    }

    /// THE-987 (AR-6) — the gain curve is monotonic across the knee: a
    /// louder input never comes out quieter, and the knee seam itself is
    /// continuous (no jump where the linear region hands over).
    #[test]
    fn soft_knee_is_monotonic_across_input_range() {
        let outs: Vec<i16> = (i16::MIN..=i16::MAX)
            .step_by(7)
            .map(|s| {
                let mut one = [s];
                apply_gain(&mut one, 2.0);
                one[0]
            })
            .collect();
        for w in outs.windows(2) {
            assert!(w[1] >= w[0], "saturator not monotonic: {w:?}");
        }
    }

    /// THE-987 (AR-6) — odd symmetry: the saturator treats positive and
    /// negative excursions identically, so a boost adds no DC offset and
    /// no even-harmonic asymmetry.
    #[test]
    fn soft_knee_is_odd_symmetric() {
        for s in (0..=i16::MAX).step_by(13).chain([i16::MAX]) {
            let (mut pos, mut neg) = ([s], [-s]);
            apply_gain(&mut pos, 2.0);
            apply_gain(&mut neg, 2.0);
            assert_eq!(
                pos[0] as i32,
                -(neg[0] as i32),
                "asymmetric saturation at ±{s}",
            );
        }
    }

    /// THE-987 (AR-6) — the bug as heard: a full-scale sine boosted to
    /// gain 2.0 used to flat-top into long runs of equal peak samples
    /// ("crunchy crackle on loud parts"). Through the soft knee the peaks
    /// stay curved — no flat-top runs.
    #[test]
    fn full_scale_sine_at_double_gain_has_no_flat_top() {
        // One 20 ms mono frame of a 997 Hz full-scale sine at 48 kHz.
        let sine: Vec<i16> = (0..960)
            .map(|n| {
                let t = n as f32 / 48_000.0;
                (i16::MAX as f32 * (2.0 * std::f32::consts::PI * 997.0 * t).sin()) as i16
            })
            .collect();
        let longest_run = |pcm: &[i16]| {
            let (mut longest, mut run) = (1usize, 1usize);
            for w in pcm.windows(2) {
                run = if w[1] == w[0] { run + 1 } else { 1 };
                longest = longest.max(run);
            }
            longest
        };
        // The old hard clamp: long flat-top runs pinned at full scale.
        let clipped: Vec<i16> = sine
            .iter()
            .map(|&s| (s as f32 * 2.0).clamp(i16::MIN as f32, i16::MAX as f32) as i16)
            .collect();
        assert!(
            longest_run(&clipped) >= 10,
            "fixture sine must flat-top under a hard clamp",
        );
        // The soft knee: peaks stay curved and short of full scale.
        let mut kneed = sine.clone();
        apply_gain(&mut kneed, 2.0);
        let run = longest_run(&kneed);
        assert!(
            run <= 2,
            "flat-top run of {run} equal samples survived the knee"
        );
        assert!(kneed.iter().all(|&s| (s as i32).abs() < i16::MAX as i32));
    }

    /// THE-987 — the AR-3 ramp composes with the knee: a ramp ending above
    /// unity lands its last stride exactly on the flat boosted apply, and
    /// every stride stays bounded while ramping through the knee.
    #[test]
    fn ramp_through_boost_soft_knees_each_stride() {
        let mut ramped = [30_000i16; 100];
        apply_gain_ramp(&mut ramped, 1.0, 2.0, 1);
        assert!(
            ramped.iter().all(|&s| (s as i32).abs() < i16::MAX as i32),
            "ramp stride reached full scale",
        );
        let mut flat = [30_000i16];
        apply_gain(&mut flat, 2.0);
        assert_eq!(
            ramped[99], flat[0],
            "ramp must land on the flat boosted apply"
        );
    }
}
