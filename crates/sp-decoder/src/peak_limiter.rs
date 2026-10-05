//! Sample-peak limiter on the summed stem mix (#184) and on the program's
//! audio (#210).
//!
//! [`StemMixReader`](crate::audio::StemMixReader) sums N streams (song stems, or the
//! original plus the dub voice). The dub sum goes over full scale when a voice
//! peak lands on a loud bed, and the old `clamp(-1, 1)` cut every such sample
//! flat, which is a hard clip that reaches FOH through VBAN. [`PeakLimiter`]
//! replaces the clamp. The `SP-program` output runs its own instance over
//! the program's audio (sp-server `program_output.rs`): a scene
//! transition's equal-power crossfade sums two sources, each already
//! under this ceiling, up to √2 above it. Both work the same way:
//!
//! - **Ceiling** [`LIMIT_CEILING`] = 0.98 (−0.18 dBFS). No sample leaves above
//!   it (up to one f32 rounding of the gain).
//! - **Stereo-linked.** One gain per frame, from the frame's highest |sample|,
//!   so the stereo image never shifts.
//! - **Instant attack, no lookahead.** The gain drops at once to what the
//!   frame needs (`ceiling / peak`), and adds no latency to the paced A/V path.
//! - **Exponential release.** The gain reduction decays by the factor
//!   `1 − 1000/(RELEASE_MS · rate)` per frame (a 50 ms time constant). That is an
//!   exact IEEE division, so the result is the same on every platform. The
//!   gain therefore rises by at most `1/release_frames` per frame (1/2400 at
//!   48 kHz).
//! - **Bit-identical at rest.** While the gain is exactly 1.0 the output is
//!   `x · 1.0 = x`. A sum at or under the ceiling with no tail pending is
//!   untouched.
//!
//! The state is the gain REDUCTION (`1 − gain`), not the gain. A gain
//! recovering in f32 toward 1.0 stalls below unity forever once its step
//! `d · (1 − R)` is under half an ulp (up to ~1200 ulp below 1.0 at 48 kHz);
//! a reduction decays freely, and once the gain rounds to exactly 1.0 the
//! state is dropped to 0.

/// The limiter's ceiling: 0.98 (−0.18 dBFS), just under full scale, so the
/// VBAN INT24 encoder's ±1.0 clamp never engages on a limited output.
pub const LIMIT_CEILING: f32 = 0.98;

/// Release time constant in ms (the reduction falls to 1/e in about 50 ms).
const RELEASE_MS: f64 = 50.0;

/// The per-frame release factor `1 − 1000/(RELEASE_MS · rate)`. A rate under
/// 20 Hz (never real audio) would give a negative factor: `max(0)` makes it an
/// instant release instead. Its own fn (not inline in `new`, which
/// cargo-mutants never mutates) so the mutation gate covers the arithmetic.
fn release_factor(sample_rate: u32) -> f32 {
    (1.0 - 1000.0 / (RELEASE_MS * f64::from(sample_rate))).max(0.0) as f32
}

/// One audio stream's peak limiter. Its owner keeps it, so its state
/// carries across blocks: the stem-mix reader (a seek resets it, a new song
/// gets a new one) and the `SP-program` output (#210: reset where the
/// program's timeline restarts).
#[derive(Debug, Clone)]
pub struct PeakLimiter {
    /// Per-frame factor the gain reduction decays by while no frame needs more.
    release: f32,
    /// Current gain reduction (`1 − gain`); 0.0 = at rest (unity gain).
    reduction: f32,
    /// Frames this limiter has scaled (gain below 1.0) since it was built.
    limited_frames: u64,
}

impl PeakLimiter {
    /// A limiter at rest for audio at `sample_rate` Hz.
    pub fn new(sample_rate: u32) -> Self {
        Self {
            release: release_factor(sample_rate),
            reduction: 0.0,
            limited_frames: 0,
        }
    }

    /// Drop any release tail: the next frame starts at unity (a seek starts
    /// unrelated audio).
    pub fn reset(&mut self) {
        self.reduction = 0.0;
    }

    /// Frames scaled so far (the reader logs it on its 1 Hz level line, the
    /// program output on its fade line).
    pub fn limited_frames(&self) -> u64 {
        self.limited_frames
    }

    /// Limit an interleaved block in place, frame by frame (`channels`
    /// samples per frame, one gain each; `channels` ≥ 1).
    pub fn process(&mut self, block: &mut [f32], channels: usize) {
        for frame in block.chunks_exact_mut(channels) {
            let peak = frame.iter().fold(0.0_f32, |m, s| m.max(s.abs()));
            // A silent frame gives `ceiling / 0 = inf`, so it needs nothing.
            let needed = 1.0 - LIMIT_CEILING / peak;
            let reduction = (self.reduction * self.release).max(needed);
            let gain = 1.0 - reduction;
            if gain < 1.0 {
                self.reduction = reduction;
                self.limited_frames += 1;
            } else {
                self.reduction = 0.0;
            }
            for s in frame.iter_mut() {
                *s *= gain;
            }
        }
    }
}

#[cfg(test)]
#[path = "peak_limiter_tests.rs"]
mod tests;
