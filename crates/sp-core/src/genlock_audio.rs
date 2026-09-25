//! Paced-audio grid math (#148) — pure, WASM-safe (integer only, no clock
//! calls). Mirrors camera-box#1294 §6: audio rides the same wall-clock grid as
//! the video, exactly `samples_per_boundary` samples per boundary.
//!
//! The PLL level trim that used to live here (`AudioPll`, `LevelAverager`,
//! `residual_ppm`) is deleted (#148 design v2): the paced audio is now pinned
//! to the picture by MEDIA TIME (`sp-server` `playback/pacer_av_align.rs` +
//! `audio_grid.rs`), which is the single controller on the audio buffer.

/// Samples delivered per grid boundary at `rate_hz` on an integer `fps` grid:
/// `rate / fps` (**1600 @ 48 kHz / 30 fps**, 800 @ 60 fps). Returns 0 for a
/// non-positive `fps` or `rate` rather than dividing by zero.
pub fn samples_per_boundary(rate_hz: i64, fps: i64) -> usize {
    if fps <= 0 || rate_hz <= 0 {
        return 0;
    }
    (rate_hz / fps) as usize
}

#[cfg(test)]
#[path = "genlock_audio_tests.rs"]
mod genlock_audio_tests;
