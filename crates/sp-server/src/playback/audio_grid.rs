//! Wall-clock audio grid buffer (#148).
//!
//! A planar-float FIFO that decouples the file audio sample clock from the
//! wall-clock grid: the decode loop `push`es every consumed frame's audio, and
//! the pacer `take_boundary_chunk`s exactly `samples_per_boundary` (1600 @
//! 48 kHz / 30 fps) samples at each grid boundary. The chunk is read through a
//! **linear-interpolation fractional pointer** whose step is
//! `1 + applied_ppm · 1e-6`, so the slow-resample correction the
//! [`AudioPll`](sp_core::genlock::audio::AudioPll) computes is applied
//! transparently (ppm-scale ratios make linear interpolation inaudible; no FFT
//! resampler, no added block latency, camera-box#1294 §6.5 — never a step or a
//! drop/insert burst).
//!
//! Underrun → ONLY the missing tail of the chunk is zero-filled (silence, never a
//! repeat of stale audio); the FIFO contents and `frac_pos` are KEPT (#148
//! rework, item 4 — clearing the whole FIFO dropped up to a boundary of valid
//! samples and guaranteed a second underrun) and `underruns` is bumped. Overflow
//! past a hard 2 s cap → the oldest audio is dropped, `overflows` is bumped, and
//! a one-per-song WARN is armed (`take_overflow_warning`).
//!
//! Pure: no clock calls, no I/O. Fully unit-tested on Linux CI.

use std::collections::VecDeque;

/// A planar-float FIFO delivering fixed-size boundary chunks on the wall grid.
pub struct AudioGridBuffer {
    /// Channel count, established on the first `push` (1–2 in practice). 0 until
    /// then (and after [`clear`](Self::clear)).
    channels: usize,
    /// Sample rate (Hz) — 48 000, enforced upstream by the decoder.
    rate: u32,
    /// One FIFO per channel (planar).
    fifo: Vec<VecDeque<f32>>,
    /// Fractional read position within the FIFO front, in input samples. Always
    /// in `[0, 1)` between takes (the integer part is drained each take).
    frac_pos: f64,
    /// Slow-resample correction (ppm): the fractional read step is
    /// `1 + applied_ppm · 1e-6`. Set by the pacer from the `AudioPll`.
    applied_ppm: f64,
    /// Nominal steady level (samples/channel) the pacer servos toward — 2
    /// boundaries (3200 @ 1600/boundary).
    target_level: usize,
    /// Hard cap (samples/channel) = 2 s; oldest audio is dropped past it.
    cap_samples: usize,
    underruns: u64,
    overflows: u64,
    /// True once an overflow WARN has been emitted this song; a latch so the log
    /// carries at most one overflow warning per song. Cleared by [`clear`].
    warned_overflow: bool,
}

impl AudioGridBuffer {
    /// A fresh buffer at `rate_hz` servoing toward `target_level` samples, with a
    /// 2-second hard cap.
    pub fn new(rate_hz: u32, target_level: usize) -> Self {
        Self {
            channels: 0,
            rate: rate_hz,
            fifo: Vec::new(),
            frac_pos: 0.0,
            applied_ppm: 0.0,
            target_level,
            cap_samples: (rate_hz as usize) * 2,
            underruns: 0,
            overflows: 0,
            warned_overflow: false,
        }
    }

    /// Append planar audio (one `Vec<f32>` per channel). The channel count is
    /// fixed on the first non-empty push; later pushes use the established count
    /// (a mismatch is clamped to the common minimum rather than desyncing).
    pub fn push(&mut self, planar: &[Vec<f32>]) {
        if planar.is_empty() {
            return;
        }
        if self.channels == 0 {
            self.channels = planar.len();
            self.fifo = (0..self.channels).map(|_| VecDeque::new()).collect();
        }
        let ch = self.channels.min(planar.len());
        let n = planar.iter().take(ch).map(|c| c.len()).min().unwrap_or(0);
        for (c, fifo) in self.fifo.iter_mut().enumerate().take(ch) {
            for &s in &planar[c][..n] {
                fifo.push_back(s);
            }
        }
        self.enforce_cap();
    }

    /// Take exactly `n` output samples per channel through the fractional reader.
    /// Zero-fills (and counts one underrun) if the FIFO runs dry mid-chunk;
    /// returns an empty `Vec` when no channels have been seen yet.
    pub fn take_boundary_chunk(&mut self, n: usize) -> Vec<Vec<f32>> {
        if self.channels == 0 || n == 0 {
            return Vec::new();
        }
        let step = (1.0 + self.applied_ppm * 1e-6).max(1e-6);
        let mut out: Vec<Vec<f32>> = (0..self.channels).map(|_| vec![0.0f32; n]).collect();
        let level = self.fifo[0].len();
        let mut starved = false;

        for j in 0..n {
            let i = self.frac_pos.floor() as usize;
            let frac = self.frac_pos - i as f64;
            let need_next = frac > 0.0;
            if i >= level || (need_next && i + 1 >= level) {
                // FIFO exhausted mid-chunk — the remainder stays silent.
                starved = true;
                break;
            }
            for (c, out_ch) in out.iter_mut().enumerate() {
                let a = self.fifo[c][i] as f64;
                out_ch[j] = if need_next {
                    let b = self.fifo[c][i + 1] as f64;
                    (a * (1.0 - frac) + b * frac) as f32
                } else {
                    a as f32
                };
            }
            self.frac_pos += step;
        }

        // Drain only the samples fully consumed by the reader; KEEP the rest of
        // the FIFO and the fractional pointer. On a starve this preserves the
        // last (interpolation-partner) samples and `frac_pos` so the next chunk
        // resumes cleanly once more audio arrives — the missing tail of THIS
        // chunk stays zero-filled (#148 rework, item 4).
        if starved {
            self.underruns += 1;
        }
        let consumed = self.frac_pos.floor() as usize;
        for ch in &mut self.fifo {
            let take_n = consumed.min(ch.len());
            ch.drain(..take_n);
        }
        self.frac_pos -= consumed as f64;
        out
    }

    /// Drop the oldest audio when the level exceeds the 2 s cap (bumps
    /// `overflows`). Dropping the front jumps the read position, so reset it.
    fn enforce_cap(&mut self) {
        let level = self.fifo.first().map(|c| c.len()).unwrap_or(0);
        if level > self.cap_samples {
            let excess = level - self.cap_samples;
            for ch in &mut self.fifo {
                let d = excess.min(ch.len());
                ch.drain(..d);
            }
            self.frac_pos = 0.0;
            self.overflows += 1;
        }
    }

    /// Samples per channel currently buffered.
    pub fn level_samples(&self) -> usize {
        self.fifo.first().map(|c| c.len()).unwrap_or(0)
    }

    /// Buffered audio in milliseconds (`level / rate`).
    pub fn buffer_ms(&self) -> u64 {
        if self.rate == 0 {
            return 0;
        }
        (self.level_samples() as u64) * 1000 / self.rate as u64
    }

    /// Set the slow-resample correction (ppm) — the pacer copies the PLL output.
    pub fn set_applied_ppm(&mut self, ppm: f64) {
        self.applied_ppm = ppm;
    }

    pub fn target_level(&self) -> usize {
        self.target_level
    }

    pub fn cap_samples(&self) -> usize {
        self.cap_samples
    }

    pub fn underruns(&self) -> u64 {
        self.underruns
    }

    pub fn overflows(&self) -> u64 {
        self.overflows
    }

    /// Returns `true` exactly once per song after the first overflow, so the
    /// caller emits a single WARN per song rather than one per dropped chunk
    /// (#148 rework, item 4). Re-armed by [`clear`](Self::clear) (anchor).
    pub fn take_overflow_warning(&mut self) -> bool {
        if self.overflows > 0 && !self.warned_overflow {
            self.warned_overflow = true;
            true
        } else {
            false
        }
    }

    /// Empty the FIFO and reset the reader + correction (called on play / seek /
    /// new song via the pacer's `anchor`). Cumulative `underruns` / `overflows`
    /// survive — they are lifetime telemetry, like the pacing counters.
    pub fn clear(&mut self) {
        self.channels = 0;
        self.fifo = Vec::new();
        self.frac_pos = 0.0;
        self.applied_ppm = 0.0;
        self.warned_overflow = false;
    }
}

#[cfg(test)]
#[path = "audio_grid_tests.rs"]
mod audio_grid_tests;
