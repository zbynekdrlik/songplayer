//! Preview-side audio stage probes (#184 round G4).
//!
//! Two of the three G4 seams live in the preview: the decode-seam TAP
//! (`preview_stream::StreamShared::offer_audio`, called through `&self` on the
//! decode thread) and the encoder's audio FEEDER (`preview_encoder`, its own
//! thread). Both measure with the shared `sp_decoder::LevelProbe` (itself on
//! `sp_core::audio_level`), so their `rms_dbfs` is directly comparable with the
//! reader's `stem-mix level` line. This module holds the tap's lock wrapper and
//! the two 1 Hz log lines:
//!
//! ```text
//! preview-tap level rms_dbfs=… samples=… blocks=… dropped=… window_ms=… stream=playlist-N
//! preview-afeed level rms_dbfs=… samples=… blocks=… pad_ms=… window_ms=… stream=playlist-N
//! ```

use std::sync::Mutex;
use std::time::Instant;

use sp_decoder::{LevelProbe, LevelReading};
use tracing::info;

use super::preview_stream::PREVIEW_AUDIO_FRAMES_PER_MS;

/// A [`LevelProbe`] for a seam that is only reachable through `&self` (the tap).
/// Never blocks: a contended lock skips the measurement (the decode thread is
/// the only writer, so in practice it is never contended).
#[derive(Debug)]
pub struct SharedLevelProbe {
    inner: Mutex<LevelProbe>,
}

impl SharedLevelProbe {
    /// A new probe whose first window starts at `now`.
    pub fn new(now: Instant) -> Self {
        Self {
            inner: Mutex::new(LevelProbe::new(now)),
        }
    }

    /// Accumulate one block and return the closed window's reading when one is
    /// due at `now` (one line per second — the caller logs it).
    pub fn record(&self, samples: &[f32], now: Instant) -> Option<LevelReading> {
        let mut p = self.inner.try_lock().ok()?;
        p.add(samples);
        p.poll(now)
    }

    /// Count one block the seam dropped (a full channel). Lands in the window
    /// open at the time of the drop.
    pub fn note_dropped(&self) {
        if let Ok(mut p) = self.inner.try_lock() {
            p.note_dropped();
        }
    }

    /// Level + samples of the still-open window (tests).
    #[cfg(test)]
    pub(crate) fn pending(&self) -> (f32, u64) {
        self.inner.lock().map(|p| p.pending()).unwrap_or((0.0, 0))
    }

    /// Keep the window open for the rest of a test (restart it an hour ahead),
    /// so a slow test run can never close it mid-assertion.
    #[cfg(test)]
    pub(crate) fn hold_window(&self) {
        let later = Instant::now() + std::time::Duration::from_secs(3600);
        if let Ok(mut p) = self.inner.lock() {
            *p = LevelProbe::new(later);
        }
    }
}

/// Milliseconds of 48 kHz interleaved-STEREO audio in `samples` samples.
pub fn stereo_samples_ms(samples: u64) -> u64 {
    samples / 2 / PREVIEW_AUDIO_FRAMES_PER_MS
}

/// The tap's 1 Hz line: the audio the decode seam OFFERED to the preview
/// (post-mix, stereo), and how many blocks the full channel dropped.
#[cfg_attr(test, mutants::skip)]
pub fn log_tap_level(stream: &str, r: &LevelReading) {
    info!(
        stream = %stream,
        "preview-tap level rms_dbfs={:.1} samples={} blocks={} dropped={} window_ms={}",
        r.rms_dbfs,
        r.samples,
        r.blocks,
        r.dropped_blocks,
        r.window_ms
    );
}

/// The feeder's 1 Hz line: the REAL audio written to ffmpeg (pad silence kept
/// out of the RMS and reported as `pad_ms`).
#[cfg_attr(test, mutants::skip)]
pub fn log_feed_level(stream: &str, r: &LevelReading) {
    info!(
        stream = %stream,
        "preview-afeed level rms_dbfs={:.1} samples={} blocks={} pad_ms={} window_ms={}",
        r.rms_dbfs,
        r.samples,
        r.blocks,
        stereo_samples_ms(r.silence_samples),
        r.window_ms
    );
}

#[cfg(test)]
#[path = "preview_audio_probe_tests.rs"]
mod tests;
