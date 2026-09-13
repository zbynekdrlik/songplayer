//! Audio clock-discipline math (#148) — pure, WASM-safe (`f64`/integer only, no
//! clock calls). Mirrors camera-box#1294 §6: audio rides the same wall-clock
//! grid as the video (exactly `samples_per_boundary` samples per boundary), and
//! a sustained file-clock-vs-wall-clock residual is slow-resampled rather than
//! stepped (§6.5).
//!
//! **Control model (rework lane, #148).** On the paced path both video AND audio
//! are consumed by WALL time by construction (the pacer presents a frame per
//! boundary and pushes exactly the audio of the frames it consumed), so the only
//! genuine residual is the file's own audio-vs-video clock disagreement (a few
//! ppm) plus bounded PTS→boundary rounding — never a fast-drifting position
//! error. The servo is therefore a SLOW TRIM with a long (60 s) measurement
//! window, never a fast position loop:
//!
//! 1. [`LevelAverager`] keeps two same-phase 60 s means of the POST-take buffer
//!    level; [`rate_residual_ppm`] turns their difference into a TRUE ppm rate
//!    residual (the sawtooth of unequal push/take chunk sizes averages out to
//!    < 1 ppm over a 60 s window), reported as `AudioStats.residual_ppm`.
//! 2. [`AudioPll`] trims the fractional-read correction (`applied_ppm`) once per
//!    60 s: a dead-banded (±50 ppm) `−residual·0.5` rate term slew-limited to
//!    ±5 ppm/update and clamped to ±500 ppm, PLUS a ±20 ppm position bias that
//!    engages only after the level has sat beyond ±2 boundaries of target for
//!    > 60 s and decays once it is back within ±1 boundary. Neither term ever
//!    steps > 5 ppm per update, so the correction is always a slow resample,
//!    never a step or a drop/insert burst.
//!
//! The pacer feeds the PLL rate term the NEGATED drift (`update(−drift, …)`): a
//! growing buffer (drift > 0, file/audio clock fast) then drives `applied_ppm`
//! POSITIVE — a faster fractional read that drains the excess (negative
//! feedback) — agreeing with the position bias, which drives a level above
//! target positive too.

use crate::genlock::UNITS_PER_SECOND;
use std::collections::VecDeque;

/// Samples delivered per grid boundary at `rate_hz` on an integer `fps` grid:
/// `rate / fps` (**1600 @ 48 kHz / 30 fps**, 800 @ 60 fps). Returns 0 for a
/// non-positive `fps` or `rate` rather than dividing by zero.
pub fn samples_per_boundary(rate_hz: i64, fps: i64) -> usize {
    if fps <= 0 || rate_hz <= 0 {
        return 0;
    }
    (rate_hz / fps) as usize
}

/// The file-clock error in ppm implied by an audio-buffer level drift: the level
/// moved from `level_ref` to `level_now` over `elapsed_100ns`, expressed as a
/// fraction of the nominal sample rate.
///
/// `(level_now − level_ref) / (rate_hz · elapsed_seconds) · 1e6`. A level that
/// GREW by 96 samples over 10 s at 48 kHz → **+200 ppm**; shrank → −200. Returns
/// 0.0 for a non-positive `elapsed_100ns` or `rate_hz` (guarded divisor).
pub fn residual_ppm(level_now: i64, level_ref: i64, elapsed_100ns: i64, rate_hz: i64) -> f64 {
    if elapsed_100ns <= 0 || rate_hz <= 0 {
        return 0.0;
    }
    let elapsed_s = elapsed_100ns as f64 / UNITS_PER_SECOND as f64;
    (level_now - level_ref) as f64 / (rate_hz as f64 * elapsed_s) * 1e6
}

/// The TRUE ppm rate residual from two same-phase 60 s means of the POST-take
/// buffer level (#148 rework, item 1): `(mean_now − mean_prev) / (rate · window)
/// · 1e6`. Positive = the buffer is GROWING (file/audio clock fast). Because the
/// two windows are the same length in the same phase, the sawtooth of unequal
/// push/take chunk sizes cancels to < 1 ppm; what remains is the genuine
/// file-clock drift. Returns 0.0 for a non-positive `window_s` or `rate_hz`.
///
/// Example: means 3200 → 3209.6 over 60 s at 48 kHz = +3.33 ppm.
pub fn rate_residual_ppm(mean_now: f64, mean_prev: f64, rate_hz: i64, window_s: f64) -> f64 {
    if window_s <= 0.0 || rate_hz <= 0 {
        return 0.0;
    }
    (mean_now - mean_prev) / (rate_hz as f64 * window_s) * 1e6
}

/// Dead-band half-width (ppm): a residual whose magnitude is at or below this is
/// ignored — the receiver ASRC absorbs a small steady residual (fleet floor
/// ≈ ±8 ppm) so SongPlayer must not chase it (camera-box#1294 §6.5).
pub const AUDIO_PLL_BAND_PPM: f64 = 50.0;

/// Proportional gain of the rate trim: each 60 s update nudges `rate_ppm` by
/// `−residual · 0.5` (before the slew clamp).
pub const AUDIO_PLL_GAIN: f64 = 0.5;

/// Maximum slew of EITHER term (rate or bias) per 60 s update (ppm): the
/// correction is always a slow trim, never a step (§6.5). The review's 🟡 on the
/// shipped 10 ppm/s slew — "doubles the gain" — is halved to this per-update cap.
pub const AUDIO_PLL_SLEW_PPM: f64 = 5.0;

/// Correction magnitude clamp (ppm): total `applied_ppm` never leaves ±500. The
/// receiver ASRC has its own authority; SongPlayer only nudges.
pub const AUDIO_PLL_MAX_PPM: f64 = 500.0;

/// The slow-trim update cadence (100-ns units, 60 s): the rate residual is
/// measured over a 60 s window and the PLL steps at most once per 60 s.
pub const AUDIO_PLL_UPDATE_100NS: i64 = 60 * UNITS_PER_SECOND;

/// Position-trim bias magnitude (ppm): when the post-take level has sat beyond
/// ±2 boundaries of target for > 60 s, a ±20 ppm bias is ramped in to walk it
/// back, decaying once the level is within ±1 boundary again.
pub const AUDIO_PLL_BIAS_PPM: f64 = 20.0;

/// A ring of POST-take buffer levels holding two same-phase 60 s windows, so the
/// rate residual can be read as `mean(last 60 s) − mean(the 60 s before)`
/// (#148 rework, item 1). Pure and WASM-safe; the pacer records one level per
/// boundary and reads the means once per 60 s.
#[derive(Clone, Debug)]
pub struct LevelAverager {
    /// Boundaries in one 60 s window (`fps · 60`, e.g. 1800 @ 30 fps).
    window: usize,
    /// The last `2 · window` post-take levels (front = oldest).
    ring: VecDeque<i64>,
}

impl LevelAverager {
    /// A ring holding two `window_boundaries`-long windows. A zero window is
    /// clamped to 1 so the means are always well-defined.
    pub fn new(window_boundaries: usize) -> Self {
        let window = window_boundaries.max(1);
        Self {
            window,
            ring: VecDeque::with_capacity(2 * window),
        }
    }

    /// Record one POST-take level; drops the oldest past `2 · window`.
    pub fn record(&mut self, post_take_level: i64) {
        self.ring.push_back(post_take_level);
        while self.ring.len() > 2 * self.window {
            self.ring.pop_front();
        }
    }

    /// True once both 60 s windows are full (`2 · window` samples recorded).
    pub fn windows_full(&self) -> bool {
        self.ring.len() >= 2 * self.window
    }

    /// Mean of the most recent `window` levels (the "now" 60 s). 0.0 until at
    /// least one window has been recorded.
    pub fn mean_now(&self) -> f64 {
        let n = self.ring.len();
        if n == 0 {
            return 0.0;
        }
        let take = self.window.min(n);
        let sum: i64 = self.ring.iter().skip(n - take).sum();
        sum as f64 / take as f64
    }

    /// Mean of the `window` levels immediately before the "now" window. 0.0 until
    /// both windows are full.
    pub fn mean_prev(&self) -> f64 {
        if !self.windows_full() {
            return 0.0;
        }
        let sum: i64 = self.ring.iter().take(self.window).sum();
        sum as f64 / self.window as f64
    }

    /// Drop all history (called on anchor / Resume via the pacer).
    pub fn clear(&mut self) {
        self.ring.clear();
    }
}

/// The slow-trim controller for the audio fractional-read rate (`applied_ppm`),
/// driven by the 60 s rate residual (rate term) and the post-take level vs
/// target (position term). Dead-banded, slew-limited, clamped — never a step or
/// a drop/insert burst.
///
/// `applied_ppm = clamp(rate_ppm + bias_ppm, ±500)`. The rate term
/// ([`update`](Self::update)) nudges `rate_ppm` toward `−residual` once per 60 s;
/// the position term ([`update_level`](Self::update_level)) ramps `bias_ppm` to
/// ±20 ppm when the level has sat beyond ±2 boundaries for > 60 s and decays it
/// once the level is back within ±1 boundary. Neither term steps > 5 ppm/update.
#[derive(Clone, Copy, Debug)]
pub struct AudioPll {
    /// Total correction (ppm) fed to the reader: `clamp(rate + bias, ±500)`.
    applied_ppm: f64,
    /// The rate-trim accumulator (the `−residual · 0.5` term).
    rate_ppm: f64,
    /// The position-trim bias.
    bias_ppm: f64,
    /// Wall clock (100 ns) of the last rate update; 0 = unseeded (first call
    /// seeds without acting so a long first `dt` never yields a giant step).
    last_rate_100ns: i64,
    /// Wall clock (100 ns) of the last position update; 0 = unseeded.
    last_bias_100ns: i64,
    /// Wall clock (100 ns) at which the level first left the ±2-boundary band and
    /// has stayed out since; `None` while inside it. Tracked every call.
    far_since_100ns: Option<i64>,
}

impl Default for AudioPll {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioPll {
    /// A fresh PLL: no correction, nothing seeded.
    pub fn new() -> Self {
        Self {
            applied_ppm: 0.0,
            rate_ppm: 0.0,
            bias_ppm: 0.0,
            last_rate_100ns: 0,
            last_bias_100ns: 0,
            far_since_100ns: None,
        }
    }

    /// The current total fractional-read correction in ppm.
    pub fn applied_ppm(&self) -> f64 {
        self.applied_ppm
    }

    /// Clear the correction and all trim state (called on play/seek/new song via
    /// the pacer's `anchor`, and on `Resume`).
    pub fn reset(&mut self) {
        self.applied_ppm = 0.0;
        self.rate_ppm = 0.0;
        self.bias_ppm = 0.0;
        self.last_rate_100ns = 0;
        self.last_bias_100ns = 0;
        self.far_since_100ns = None;
    }

    fn recompute(&mut self) {
        self.applied_ppm =
            (self.rate_ppm + self.bias_ppm).clamp(-AUDIO_PLL_MAX_PPM, AUDIO_PLL_MAX_PPM);
    }

    /// Rate term (#148 rework, item 2). Acts only when `now − last ≥ 60 s`
    /// (seeds `last` on the first call, no action then): if `|residual| > 50`,
    /// `rate_ppm += clamp(−residual · 0.5, ±5)`, clamped to ±500. Returns the
    /// updated total `applied_ppm`.
    pub fn update(&mut self, residual_ppm: f64, now_100ns: i64) -> f64 {
        if self.last_rate_100ns == 0 {
            self.last_rate_100ns = now_100ns;
            return self.applied_ppm;
        }
        if now_100ns - self.last_rate_100ns < AUDIO_PLL_UPDATE_100NS {
            return self.applied_ppm;
        }
        self.last_rate_100ns = now_100ns;
        if residual_ppm.abs() > AUDIO_PLL_BAND_PPM {
            let step =
                (-residual_ppm * AUDIO_PLL_GAIN).clamp(-AUDIO_PLL_SLEW_PPM, AUDIO_PLL_SLEW_PPM);
            self.rate_ppm = (self.rate_ppm + step).clamp(-AUDIO_PLL_MAX_PPM, AUDIO_PLL_MAX_PPM);
        }
        self.recompute();
        self.applied_ppm
    }

    /// Position term (#148 rework, item 2). `target = 2 · samples_per_boundary`,
    /// so `|level − target| > target` is "beyond 2 boundaries" and
    /// `|level − target| ≤ target/2` is "within 1 boundary". The far-sustain is
    /// tracked EVERY call; the bias ramps/decays only on the 60 s tick, by
    /// ≤ 5 ppm/update, toward ±20 ppm (sign to move the level back) when the
    /// level has been far for > 60 s, or toward 0 once it is near again. Returns
    /// the updated total `applied_ppm`.
    pub fn update_level(&mut self, post_take_level: i64, target: i64, now_100ns: i64) -> f64 {
        let dev = (post_take_level - target).abs() as f64;
        let far = dev > target as f64;
        let near = dev <= target as f64 / 2.0;
        if far {
            self.far_since_100ns.get_or_insert(now_100ns);
        } else if near {
            self.far_since_100ns = None;
        }
        // In the hysteresis band (target/2 < |dev| ≤ target) keep the current
        // far_since so an engaged trim rides through it back to "near".

        if self.last_bias_100ns == 0 {
            self.last_bias_100ns = now_100ns;
            return self.applied_ppm;
        }
        if now_100ns - self.last_bias_100ns < AUDIO_PLL_UPDATE_100NS {
            return self.applied_ppm;
        }
        self.last_bias_100ns = now_100ns;

        let engaged = self
            .far_since_100ns
            .map_or(false, |s| now_100ns - s > AUDIO_PLL_UPDATE_100NS);
        let bias_target = if engaged {
            AUDIO_PLL_BIAS_PPM * (post_take_level - target).signum() as f64
        } else if near {
            0.0
        } else {
            self.bias_ppm
        };
        let bstep = (bias_target - self.bias_ppm).clamp(-AUDIO_PLL_SLEW_PPM, AUDIO_PLL_SLEW_PPM);
        self.bias_ppm += bstep;
        self.recompute();
        self.applied_ppm
    }
}

#[cfg(test)]
#[path = "genlock_audio_tests.rs"]
mod genlock_audio_tests;
