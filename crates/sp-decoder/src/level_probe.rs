//! 1 Hz audio stage-level probe (#184 round G4).
//!
//! A [`LevelProbe`] sits at ONE seam of the audio path (the `StemMixReader`
//! output, the preview tap, the preview encoder's audio feeder) and turns the
//! samples passing it into one reading per [`PROBE_INTERVAL`]: RMS level, sample
//! and block counts, plus the silence written and blocks dropped at that seam.
//! The caller logs the reading; comparing the three seams' lines for the same
//! second shows WHERE a fader change stops being heard.
//!
//! Hot-path cost: [`LevelProbe::add`] is a running sum of squares + counters (no
//! allocation); [`LevelProbe::poll`] is one `Instant` comparison. The clock is
//! passed in (`now`), so the window logic is deterministic under test.

use std::time::{Duration, Instant};

use sp_core::audio_level::LevelWindow;

/// How often a probe emits a reading — one log line per second per stream.
pub const PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// One window's worth of measurements from a [`LevelProbe`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LevelReading {
    /// RMS level in dBFS of the samples added in the window (the floor
    /// `sp_core::audio_level::SILENCE_FLOOR_DBFS` when none / all zero).
    pub rms_dbfs: f32,
    /// Interleaved samples added (every channel sample counts once).
    pub samples: u64,
    /// Number of [`LevelProbe::add`] calls (blocks / packets) in the window.
    pub blocks: u64,
    /// Interleaved samples of SILENCE the seam wrote instead of real audio —
    /// counted, never mixed into `rms_dbfs`.
    pub silence_samples: u64,
    /// Blocks the seam DROPPED (e.g. a full channel) in the window.
    pub dropped_blocks: u64,
    /// The window's actual length in ms (≥ [`PROBE_INTERVAL`]).
    pub window_ms: u64,
}

/// Per-seam accumulator with a 1 Hz window. See the module docs.
#[derive(Debug, Clone)]
pub struct LevelProbe {
    window: LevelWindow,
    blocks: u64,
    silence_samples: u64,
    dropped_blocks: u64,
    window_start: Instant,
}

impl LevelProbe {
    /// A new, empty probe whose first window starts at `now`.
    pub fn new(now: Instant) -> Self {
        Self {
            window: LevelWindow::default(),
            blocks: 0,
            silence_samples: 0,
            dropped_blocks: 0,
            window_start: now,
        }
    }

    /// Accumulate one block of real (interleaved) audio.
    pub fn add(&mut self, samples: &[f32]) {
        self.window.add(samples);
        self.blocks += 1;
    }

    /// Count `samples` interleaved samples of silence the seam wrote in place of
    /// audio (kept out of the RMS, so a padded second never reads as quiet audio).
    pub fn add_silence(&mut self, samples: u64) {
        self.silence_samples += samples;
    }

    /// Count one block the seam dropped.
    pub fn note_dropped(&mut self) {
        self.dropped_blocks += 1;
    }

    /// The level + sample count accumulated so far in the current window,
    /// without closing it.
    pub fn pending(&self) -> (f32, u64) {
        (self.window.rms_dbfs(), self.window.samples())
    }

    /// Restart an IDLE window: if no block was added and the window is already
    /// overdue at `now`, re-open it at `now`. A seam that only measures while it
    /// is used (the preview tap, only while watched) calls this before
    /// [`LevelProbe::add`], so the first line after an idle gap covers the
    /// second it measured instead of the whole gap (`window_ms` stays honest).
    pub fn restart_if_idle(&mut self, now: Instant) {
        if self.blocks == 0 && now.saturating_duration_since(self.window_start) >= PROBE_INTERVAL {
            self.window_start = now;
        }
    }

    /// Close the window if [`PROBE_INTERVAL`] has elapsed since it opened:
    /// return its reading and start the next window at `now`. `None` while the
    /// window is still open.
    pub fn poll(&mut self, now: Instant) -> Option<LevelReading> {
        let elapsed = now.saturating_duration_since(self.window_start);
        if elapsed < PROBE_INTERVAL {
            return None;
        }
        let (rms_dbfs, samples) = self.window.take();
        let reading = LevelReading {
            rms_dbfs,
            samples,
            blocks: self.blocks,
            silence_samples: self.silence_samples,
            dropped_blocks: self.dropped_blocks,
            window_ms: elapsed.as_millis() as u64,
        };
        self.blocks = 0;
        self.silence_samples = 0;
        self.dropped_blocks = 0;
        self.window_start = now;
        Some(reading)
    }
}

#[cfg(test)]
#[path = "level_probe_tests.rs"]
mod tests;
