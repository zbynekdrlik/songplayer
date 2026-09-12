//! Genlock wall-clock boundary math (#146).
//!
//! Pure integer arithmetic, WASM-safe: this module makes **no** clock calls
//! (`Instant::now` / `SystemTime::now` are forbidden in `sp-core`) — callers
//! supply the wall-clock reading. The functions mirror camera-box#1294 §3
//! (grid) and §4 (video timecode) 1:1 in both algorithm and rounding, so the
//! contract's reference vectors transfer directly (see `genlock_tests.rs`).
//!
//! **FLOOR, never ceil.** The video timecode is the greatest grid boundary at
//! or before the present wall time. A future-dated (ceil) stamp arms the
//! receiver's backward-step guard — the 2026-08-07 −900 ms hold collapse
//! (camera-box#1009/#1007, measured margin 0.3 ms at trigger). See
//! [`floor_boundary_100ns`].

/// 100-ns units per second (the NDI timecode unit).
pub const UNITS_PER_SECOND: i64 = 10_000_000;

/// Re-sample the monotonic-to-realtime offset at least every this many frames
/// (camera-box `OFFSET_RESAMPLE_INTERVAL_FRAMES`).
pub const OFFSET_RESAMPLE_INTERVAL_FRAMES: u64 = 100;

/// The nominal grid rate SongPlayer stamps against — the receiving canvas
/// rate (cg OBS = 30 fps), camera-box#1294 §3. Per-output overrides are a
/// later ticket (#147, open question 1).
pub const GENLOCK_GRID_FPS: i64 = 30;

/// Length of one grid interval in 100-ns units: `1e7 / fps`.
///
/// `30 fps -> 333_333`, `60 fps -> 166_666`. Returns 0 for a non-positive
/// `fps` rather than dividing by zero.
pub fn interval_100ns(fps: i64) -> i64 {
    if fps <= 0 {
        return 0;
    }
    UNITS_PER_SECOND / fps
}

/// The genlock video timecode: the greatest grid boundary `<= now_100ns`, in
/// 100-ns units since the Unix epoch. camera-box `src/ndi.rs:78-97`.
///
/// Algorithm: anchor to the containing second, recover the slot by integer
/// division, then apply the load-bearing promotion fix. Because `1e7/fps` is
/// truncated (`b1 @ 30 = 333_333`, below the exact rational `333_333.33`),
/// naive slot recovery under-counts on an exact boundary; the promotion adds
/// the missing slot back. A non-positive `fps` returns `now_100ns` unchanged.
pub fn floor_boundary_100ns(now_100ns: i64, fps: i64) -> i64 {
    if fps <= 0 {
        return now_100ns;
    }
    // Start of the containing second.
    let cs = (now_100ns / UNITS_PER_SECOND) * UNITS_PER_SECOND;
    let off = now_100ns - cs;
    let mut slot = off * fps / UNITS_PER_SECOND;
    // Promotion fix: if the next slot's boundary is still <= off, we
    // under-counted by one (integer-truncated interval).
    if (slot + 1) * UNITS_PER_SECOND / fps <= off {
        slot += 1;
    }
    cs + slot * UNITS_PER_SECOND / fps
}

/// Round a rational frame rate `n/d` to the nearest integer grid rate
/// (59.94 -> 60, 29.97 -> 30). camera-box `src/ndi.rs:133-139`.
///
/// Returns 0 when `d == 0`. Truncation here would silently drift the grid to
/// 59/29 — the defect this round guards against.
pub fn fps_from_frame_rate(n: i64, d: i64) -> i64 {
    if d == 0 {
        return 0;
    }
    (n + d / 2) / d
}

/// Realtime timecode for a captured frame: `mono + offset`, saturating so a
/// clock at the i64 extremes never wraps. camera-box capture path.
pub fn capture_realtime_100ns(mono: i64, offset: i64) -> i64 {
    mono.saturating_add(offset)
}

/// Whether the monotonic-to-realtime offset should be re-sampled, given how
/// many frames have elapsed since the last resample (`>= 100`, camera-box
/// `OFFSET_RESAMPLE_INTERVAL_FRAMES`).
pub fn should_resample_mono_to_real_offset(frames_since: u64) -> bool {
    frames_since >= OFFSET_RESAMPLE_INTERVAL_FRAMES
}

#[cfg(test)]
#[path = "genlock_tests.rs"]
mod genlock_tests;
