//! Audio clock-discipline math (#148) — pure, WASM-safe (`f64`/integer only, no
//! clock calls). Mirrors camera-box#1294 §6: audio rides the same wall-clock
//! grid as the video (exactly `samples_per_boundary` samples per boundary), and
//! a sustained file-clock-vs-wall-clock residual is slow-resampled rather than
//! stepped (§6.5).
//!
//! **Control model.** Once the sink submits exactly `samples_per_boundary`
//! samples per wall-clock boundary, the DELIVERY rate is locked to the wall by
//! construction — so a growing/shrinking audio buffer level IS the file sample
//! clock's error against the wall. [`residual_ppm`] converts that level drift to
//! a ppm error; [`AudioPll`] one-poles a fractional-read correction toward
//! `−residual` (only after the error has stayed outside a ±50 ppm dead-band for
//! a 10 s hold, clamped to ±500 ppm, never a step > 10 ppm per 1 s update), so
//! the residual converges without a drop/insert burst. The caller (the pacer)
//! feeds the residual so the correction is negative feedback — see the pacer's
//! `audio_pll_update` for the sign convention.

use crate::genlock::UNITS_PER_SECOND;

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

/// Dead-band half-width (ppm): a residual whose magnitude is at or below this is
/// ignored — the receiver ASRC absorbs a small steady residual (fleet floor
/// ≈ ±8 ppm) so SongPlayer must not chase it (camera-box#1294 §6.5).
pub const AUDIO_PLL_BAND_PPM: f64 = 50.0;

/// How long (100-ns units, 10 s) the residual must stay CONTINUOUSLY outside the
/// dead-band before the PLL starts correcting. Short excursions (a scheduling
/// hiccup) never move the applied correction.
pub const AUDIO_PLL_HOLD_100NS: i64 = 100_000_000;

/// One-pole time constant (100-ns units, 10 s): the correction moves a fraction
/// `dt / tau` of the remaining error per update.
pub const AUDIO_PLL_TAU_100NS: i64 = 100_000_000;

/// Correction magnitude clamp (ppm): `applied_ppm` never leaves ±500. The
/// receiver ASRC has its own authority; SongPlayer only nudges.
pub const AUDIO_PLL_MAX_PPM: f64 = 500.0;

/// Maximum slew of `applied_ppm` per second — the correction never steps more
/// than this in one 1-s update, so it is always a slow resample, never a jump
/// (§6.5). At 10 ppm/s a full 0→−200 ppm correction takes ~50 s (well under the
/// 60 s convergence budget on #148).
pub const AUDIO_PLL_SLEW_PPM_PER_S: f64 = 10.0;

/// A one-pole slow-resample controller for the audio fractional-read rate
/// (`applied_ppm`), driven by the buffer-level residual. Dead-band + hold +
/// slew-limited + clamped, so it never applies a step or a drop/insert burst.
///
/// `update(residual, now)` is called once per second with the current residual
/// (ppm). While `|residual| ≤ band` the correction holds; once the residual has
/// stayed outside the band for `hold`, `applied_ppm` one-poles toward
/// `−residual`, moving at most [`AUDIO_PLL_SLEW_PPM_PER_S`] ppm per second and
/// clamped to ±[`AUDIO_PLL_MAX_PPM`].
#[derive(Clone, Copy, Debug)]
pub struct AudioPll {
    applied_ppm: f64,
    /// Wall clock (100 ns) at which the residual first left the band and has
    /// stayed out since; `None` while inside the band.
    outside_since_100ns: Option<i64>,
    /// Wall clock (100 ns) of the previous update, for the one-pole `dt`.
    last_100ns: i64,
}

impl Default for AudioPll {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioPll {
    /// A fresh PLL: no correction, no hold started.
    pub fn new() -> Self {
        Self {
            applied_ppm: 0.0,
            outside_since_100ns: None,
            last_100ns: 0,
        }
    }

    /// The current fractional-read correction in ppm.
    pub fn applied_ppm(&self) -> f64 {
        self.applied_ppm
    }

    /// Clear the correction and the hold state (called on play/seek/new song).
    pub fn reset(&mut self) {
        self.applied_ppm = 0.0;
        self.outside_since_100ns = None;
        self.last_100ns = 0;
    }

    /// Feed one residual sample (ppm) at wall time `now_100ns`; returns the
    /// updated `applied_ppm`. See the type docs for the control law.
    pub fn update(&mut self, residual_ppm: f64, now_100ns: i64) -> f64 {
        let dt = (now_100ns - self.last_100ns).max(0);
        self.last_100ns = now_100ns;

        // Dead-band: ignore a residual the receiver ASRC absorbs; reset the hold.
        if residual_ppm.abs() <= AUDIO_PLL_BAND_PPM {
            self.outside_since_100ns = None;
            return self.applied_ppm;
        }

        // Hold: only act after the residual has stayed out-of-band continuously.
        let since = *self.outside_since_100ns.get_or_insert(now_100ns);
        if now_100ns - since < AUDIO_PLL_HOLD_100NS {
            return self.applied_ppm;
        }

        // One-pole toward −residual, slew-limited and clamped.
        let target = -residual_ppm;
        let factor = (dt as f64 / AUDIO_PLL_TAU_100NS as f64).clamp(0.0, 1.0);
        let mut delta = (target - self.applied_ppm) * factor;
        let max_step = AUDIO_PLL_SLEW_PPM_PER_S * (dt as f64 / UNITS_PER_SECOND as f64);
        delta = delta.clamp(-max_step, max_step);
        self.applied_ppm = (self.applied_ppm + delta).clamp(-AUDIO_PLL_MAX_PPM, AUDIO_PLL_MAX_PPM);
        self.applied_ppm
    }
}

#[cfg(test)]
#[path = "genlock_audio_tests.rs"]
mod genlock_audio_tests;
