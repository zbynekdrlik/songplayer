//! Audio level measurement shared by every stage probe (#184 round G4).
//!
//! The stage probes (the `StemMixReader` output, the preview tap, the preview
//! encoder's audio feeder) each report the RMS level of the audio that passed
//! them in the last second, so the box log shows at WHICH seam a fader change
//! stops being heard. They all measure through this one pure module so the
//! three numbers are directly comparable.
//!
//! WASM-safe: pure `f32`/`f64` arithmetic, no I/O, no clock. The 1 Hz window
//! timing lives with the callers (`sp_decoder::level_probe`).

/// The level reported for silence (all-zero or no samples): a finite floor far
/// below any audible signal, so a log line never carries `-inf` / `NaN`.
/// −180 dBFS is below the noise floor of 32-bit float audio (≈ −150 dBFS for
/// 24-bit), so a real signal never reads as the floor.
pub const SILENCE_FLOOR_DBFS: f32 = -180.0;

/// Convert a mean square (`Σx² / n`, full scale = 1.0) to dBFS, clamped to
/// [`SILENCE_FLOOR_DBFS`]. `10·log10(0) = −inf` and `NaN` both land on the
/// floor (`f32::max` returns the non-NaN operand).
pub fn dbfs_from_mean_square(mean_square: f64) -> f32 {
    ((10.0 * mean_square.log10()) as f32).max(SILENCE_FLOOR_DBFS)
}

/// RMS level of `samples` in dBFS (full scale ±1.0 = 0 dBFS; a full-scale sine
/// ≈ −3.01 dBFS). An empty or all-zero slice returns [`SILENCE_FLOOR_DBFS`].
pub fn rms_dbfs(samples: &[f32]) -> f32 {
    let mut w = LevelWindow::default();
    w.add(samples);
    w.rms_dbfs()
}

/// Running RMS accumulator over a window of samples: a sum of squares plus a
/// sample count, so the hot path adds without allocating and the level is only
/// computed when a window is read. [`LevelWindow::take`] reads AND resets it.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct LevelWindow {
    sum_sq: f64,
    samples: u64,
}

impl LevelWindow {
    /// Accumulate `samples` (interleaved; every channel sample counts once).
    pub fn add(&mut self, samples: &[f32]) {
        for &s in samples {
            let v = f64::from(s);
            self.sum_sq += v * v;
        }
        self.samples += samples.len() as u64;
    }

    /// Samples accumulated since the last [`LevelWindow::take`].
    pub fn samples(&self) -> u64 {
        self.samples
    }

    /// RMS level of the accumulated samples in dBFS ([`SILENCE_FLOOR_DBFS`]
    /// when the window is empty or silent). Does not reset.
    pub fn rms_dbfs(&self) -> f32 {
        if self.samples == 0 {
            return SILENCE_FLOOR_DBFS;
        }
        dbfs_from_mean_square(self.sum_sq / self.samples as f64)
    }

    /// Read the window as `(rms_dbfs, samples)` and reset it to empty, so the
    /// next window starts from zero.
    pub fn take(&mut self) -> (f32, u64) {
        let reading = (self.rms_dbfs(), self.samples);
        *self = Self::default();
        reading
    }
}

#[cfg(test)]
#[path = "audio_level_tests.rs"]
mod tests;
