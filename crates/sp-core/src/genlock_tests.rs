//! Reference test vectors for `sp_core::genlock`.
//!
//! Every number is transcribed 1:1 from the camera-box#1294 contract digest
//! §B (compiled for songplayer #146). Wired via
//! `#[cfg(test)] #[path = "genlock_tests.rs"] mod genlock_tests;` from
//! `genlock.rs`, so `super::*` resolves to the genlock module under test.

use super::*;

// ---- constants (digest §B header) ----

#[test]
fn constants_match_contract() {
    assert_eq!(UNITS_PER_SECOND, 10_000_000);
    assert_eq!(OFFSET_RESAMPLE_INTERVAL_FRAMES, 100);
    assert_eq!(GENLOCK_GRID_FPS, 30);
}

// ---- B: interval_100ns ----

#[test]
fn interval_100ns_matches_grid() {
    assert_eq!(interval_100ns(30), 333_333);
    assert_eq!(interval_100ns(60), 166_666);
}

// ---- B1: floor_boundary_100ns (the normative video-stamp fn) ----

#[test]
fn floor_boundary_b1_vectors() {
    // (now_100ns, fps) -> expected boundary in 100 ns units since the epoch.
    let cases: [(i64, i64, i64); 8] = [
        (0, 30, 0),
        (333_333, 30, 333_333),       // exactly ON b1
        (333_332, 30, 0),             // one tick below b1
        (333_334, 30, 333_333),       // one tick above b1
        (10_000_000, 30, 10_000_000), // exact second boundary
        (9_999_999, 30, 9_666_666),   // = 29 * 1e7 / 30
        (12_345, 0, 12_345),          // fps == 0  -> passthrough
        (12_345, -5, 12_345),         // fps  < 0  -> passthrough
    ];
    for (now, fps, expected) in cases {
        assert_eq!(
            floor_boundary_100ns(now, fps),
            expected,
            "floor_boundary_100ns({now}, {fps})"
        );
    }
}

/// The floor invariant (digest §B1): for every instant across a second, the
/// returned boundary is never in the future (`b <= off`) and is never more
/// than one interval behind (`off - b <= 1e7/fps`). FLOOR, never ceil.
#[test]
fn floor_boundary_invariant_holds_across_a_second() {
    for fps in [30_i64, 60] {
        let interval = 10_000_000 / fps;
        let mut off = 0_i64;
        while off < 10_000_000 {
            let b = floor_boundary_100ns(off, fps);
            assert!(
                b <= off,
                "boundary must never be in the future: off={off} b={b} fps={fps}"
            );
            assert!(
                off - b <= interval,
                "gap must be <= one interval: off={off} b={b} fps={fps} interval={interval}"
            );
            off += 97_531;
        }
    }
}

// ---- B2: fps_from_frame_rate (round-to-nearest grid rate) ----

#[test]
fn fps_from_frame_rate_b2_vectors() {
    assert_eq!(fps_from_frame_rate(60, 1), 60);
    assert_eq!(fps_from_frame_rate(30, 1), 30);
    assert_eq!(fps_from_frame_rate(60000, 1001), 60); // 59.94 -> 60
    assert_eq!(fps_from_frame_rate(30000, 1001), 30); // 29.97 -> 30
    assert_eq!(fps_from_frame_rate(60, 0), 0); //         d == 0 -> 0
}

// ---- B4: capture_realtime_100ns (saturating add) ----

#[test]
fn capture_realtime_b4_saturating_vectors() {
    assert_eq!(capture_realtime_100ns(500, 1_000), 1_500);
    assert_eq!(capture_realtime_100ns(-200, 1_000), 800);
    assert_eq!(capture_realtime_100ns(i64::MAX, 10), i64::MAX);
}

// ---- B7: should_resample_mono_to_real_offset ----

#[test]
fn should_resample_b7_vectors() {
    assert!(!should_resample_mono_to_real_offset(0));
    assert!(!should_resample_mono_to_real_offset(99));
    assert!(should_resample_mono_to_real_offset(100));
    assert!(should_resample_mono_to_real_offset(150));
}
