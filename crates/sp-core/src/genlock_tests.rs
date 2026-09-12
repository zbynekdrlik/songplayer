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

// ===========================================================================
// Boundary-paced emission vectors (#147) — digest §B, ported 1:1 from
// camera-box `src/genlock_pacing.rs`. Pacing works in wall-clock ns.
// ===========================================================================

/// 30 fps pacing interval in ns (`I30 = 1e9 / 30`).
const I30: i64 = 33_333_333;

#[test]
fn constants_and_interval_ns_match_contract() {
    assert_eq!(GENLOCK_MAX_CATCHUP_INTERVALS, 8);
    assert_eq!(NANOS_PER_SECOND, 1_000_000_000);
    assert_eq!(interval_ns(30), I30);
    assert_eq!(interval_ns(60), 16_666_666);
    assert_eq!(interval_ns(0), 0);
    assert_eq!(interval_ns(-5), 0);
}

// ---- B3: next_boundary_100ns (the CEIL twin — sleep target only) ----

#[test]
fn next_boundary_100ns_b3_vectors() {
    assert_eq!(next_boundary_100ns(0, 30), 333_333);
    assert_eq!(next_boundary_100ns(1, 60), 166_666);
    assert_eq!(next_boundary_100ns(9_900_000, 60), 10_000_000);
    assert_eq!(next_boundary_100ns(12_345, 0), 12_345); // fps <= 0 passthrough
}

// ---- B8: genlock_emit_gate (14 vectors) ----

#[test]
fn emit_gate_b8_1_init_never_emits() {
    let now = 5 * I30 + 1000;
    let (emit, next) = genlock_emit_gate(now, 0, I30, false);
    assert!(!emit);
    assert_eq!(next, now - (now % I30) + I30);
    assert_eq!(next, 6 * I30);
}

#[test]
fn emit_gate_b8_2_boundary_unmoved_before_boundary() {
    let (emit, next) = genlock_emit_gate(10 * I30 - 5, 10 * I30, I30, false);
    assert!(!emit);
    assert_eq!(next, 10 * I30);
}

#[test]
fn emit_gate_b8_3_at_boundary_emits() {
    let (emit, next) = genlock_emit_gate(7 * I30, 7 * I30, I30, false);
    assert!(emit);
    assert_eq!(next, 8 * I30);
}

#[test]
fn emit_gate_b8_4_just_after_boundary_emits() {
    let (emit, next) = genlock_emit_gate(7 * I30 + 100, 7 * I30, I30, false);
    assert!(emit);
    assert_eq!(next, 8 * I30);
}

#[test]
fn emit_gate_b8_5_zero_interval_no_panic() {
    let (emit, next) = genlock_emit_gate(123_456_789, 0, 0, false);
    assert!(!emit);
    assert_eq!(next, 0);
}

#[test]
fn emit_gate_b8_6_zero_interval_keeps_boundary() {
    let (emit, next) = genlock_emit_gate(999, 555, 0, false);
    assert!(!emit);
    assert_eq!(next, 555);
}

#[test]
fn emit_gate_b8_7_misaligned_advance_is_not_resync() {
    let b = 7 * I30 + 5;
    let (emit, next) = genlock_emit_gate(b, b, I30, false);
    assert!(emit);
    assert_eq!(next, b + I30);
    // Must NOT equal the resync-realigned value.
    assert_ne!(next, b - (b % I30) + I30);
}

#[test]
fn emit_gate_b8_8_lag_twelve_resyncs() {
    let b = 3 * I30;
    let now = b + 12 * I30 + 17;
    let (emit, next) = genlock_emit_gate(now, b, I30, false);
    assert!(emit);
    assert_eq!(next, now - (now % I30) + I30);
}

#[test]
fn emit_gate_b8_9_lag_equal_bound_catches_up() {
    // lag == 8 (the bound) still catches up: `>` is the resync gate.
    let b = 7 * I30 + 5;
    let now = b + 8 * I30 + 11;
    let (emit, next) = genlock_emit_gate(now, b, I30, false);
    assert!(emit);
    assert_eq!(next, b + I30);
}

#[test]
fn emit_gate_b8_10_backward_step_relatches() {
    let b = 100 * I30;
    let now = b - 90 * I30;
    let (_emit, next) = genlock_emit_gate(now, b, I30, false);
    assert!(next <= now + I30);
    assert_ne!(next, b);
}

#[test]
fn emit_gate_b8_11_buffered_never_resyncs() {
    let b = 7 * I30 + 5;
    let now = b + 11 * I30 + 11;
    let (emit, next) = genlock_emit_gate(now, b, I30, true);
    assert!(emit);
    assert_eq!(next, b + I30);
}

#[test]
fn emit_gate_b8_12_sixty_fps_decimates_to_thirty() {
    let cap_interval = 16_666_666_i64;
    let mut next_b = 0_i64;
    let mut emitted = 0;
    let start = 1_000_000_000_i64;
    for k in 0..60 {
        let now = start + k * cap_interval;
        let (emit, nb) = genlock_emit_gate(now, next_b, I30, false);
        next_b = nb;
        if emit {
            emitted += 1;
        }
    }
    assert!((29..=31).contains(&emitted), "got {emitted}");
}

#[test]
fn emit_gate_b8_13_buffered_drain_four_of_four() {
    let b0 = 10 * I30;
    let resume = b0 + 4 * I30;
    let mut next_b = b0;
    let mut emitted = 0;
    for k in 0..4 {
        let now = resume + k;
        let (emit, nb) = genlock_emit_gate(now, next_b, I30, false);
        next_b = nb;
        if emit {
            emitted += 1;
        }
    }
    assert_eq!(emitted, 4, "every buffered frame in the drain must emit");
}

#[test]
fn emit_gate_b8_14_buffered_drain_six_of_six_zero_skip() {
    let b0 = 100 * I30;
    let resume = b0 + 10 * I30;
    let mut next_b = b0;
    let mut emitted = 0;
    let mut total_skip = 0;
    for k in 0..6 {
        let now = resume + k;
        let prev = next_b;
        let (emit, nb) = genlock_emit_gate(now, next_b, I30, true); // queue non-empty
        total_skip += boundary_skip_count(prev, nb, I30);
        next_b = nb;
        if emit {
            emitted += 1;
        }
    }
    assert_eq!(emitted, 6);
    assert_eq!(total_skip, 0);
}

// ---- B9: genlock_emit_on_time (7 vectors) ----

#[test]
fn emit_on_time_b9_vectors() {
    let b7 = 7 * I30;
    assert!(genlock_emit_on_time(b7, b7, I30));
    assert!(genlock_emit_on_time(b7 + 5, b7, I30));
    assert!(genlock_emit_on_time(b7 + I30 - 1, b7, I30));
    let b10 = 10 * I30;
    assert!(!genlock_emit_on_time(b10 - 5, b10, I30));
    assert!(!genlock_emit_on_time(b10 + I30, b10, I30));
    assert!(!genlock_emit_on_time(b10 + 3 * I30, b10, I30));
    assert!(!genlock_emit_on_time(12_345, 6_789, 0));
}

// ---- B10: genlock_lag_intervals (7 vectors + equivalence) ----

#[test]
fn lag_intervals_b10_vectors() {
    let b = 9 * I30;
    assert_eq!(genlock_lag_intervals(b - 5, b, I30), 0);
    assert_eq!(genlock_lag_intervals(b, b, I30), 0);
    assert_eq!(genlock_lag_intervals(b + I30 - 1, b, I30), 0);
    assert_eq!(genlock_lag_intervals(b + I30, b, I30), 1);
    assert_eq!(genlock_lag_intervals(b + 2 * I30 + 7, b, I30), 2);
    assert_eq!(genlock_lag_intervals(b + 5 * I30, b, I30), 5);
    assert_eq!(genlock_lag_intervals(12_345, 6_789, 0), 0);
}

#[test]
fn lag_zero_iff_on_time_or_before_boundary_b10() {
    let b = 6 * I30;
    for delta in [-3_i64, 0, 5, I30 - 1, I30, 3 * I30] {
        let now = b + delta;
        let lag = genlock_lag_intervals(now, b, I30);
        let on_time = genlock_emit_on_time(now, b, I30);
        let before = now < b;
        assert_eq!(lag == 0, on_time || before, "delta={delta}");
    }
}

// ---- B11: boundary_skip_count (6 vectors) ----

#[test]
fn boundary_skip_count_b11_vectors() {
    assert_eq!(boundary_skip_count(0, 100 * I30, I30), 0); // old == 0 sentinel
    assert_eq!(boundary_skip_count(5 * I30, 5 * I30, I30), 0); // unchanged
    assert_eq!(boundary_skip_count(5 * I30, 6 * I30, I30), 0); // one interval
    assert_eq!(boundary_skip_count(10 * I30, 16 * I30, I30), 5); // 6-interval leap
    assert_eq!(boundary_skip_count(100 * I30, 40 * I30, I30), 0); // backward step
    assert_eq!(boundary_skip_count(5 * I30, 200 * I30, 0), 0); // interval 0
}

// ---- B12: starvation_repeat_timecode_100ns (5 vectors) ----

#[test]
fn starvation_repeat_timecode_b12_vectors() {
    let base = 123_456_789_i64;
    assert_eq!(
        starvation_repeat_timecode_100ns(base, 1, 60),
        base - 166_666
    );
    assert_eq!(
        starvation_repeat_timecode_100ns(base, 2, 60),
        base - 333_332
    );
    assert_eq!(
        starvation_repeat_timecode_100ns(base, 4, 60),
        base - 666_664
    );
    assert_eq!(starvation_repeat_timecode_100ns(999, 3, 0), 999);
    assert_eq!(starvation_repeat_timecode_100ns(999, 3, -5), 999);
}

// ===========================================================================
// Exact-100-ns-grid emission (#147 rework) — the twin the `Pacer` actually
// uses (SongPlayer generates its own timing, so pacing + stamps share ONE
// grid). The 14 B8 scenarios are mirrored on the exact grid with values
// computed from the helpers (`b(k) = k * 1e7 / 30` is the k-th grid boundary),
// plus a second-crossing case and the on-grid / never-stale invariants.
// ===========================================================================

/// The k-th exact-rational grid boundary at 30 fps, in 100-ns units.
fn b(k: i64) -> i64 {
    k * UNITS_PER_SECOND / 30
}

#[test]
fn strict_next_boundary_100ns_always_advances_even_on_grid() {
    // `next_boundary_100ns` is idempotent on the promotion boundaries it
    // produces; `strict_next` must still advance exactly one slot.
    assert_eq!(strict_next_boundary_100ns(0, 30), 333_333); // b0 -> b1
    assert_eq!(strict_next_boundary_100ns(333_333, 30), 666_666); // b1 -> b2 (promotion)
    assert_eq!(strict_next_boundary_100ns(2_333_333, 30), 2_666_666); // b7 -> b8 (promotion)
    // Off-grid input -> the next boundary strictly above it (no over-shoot).
    assert_eq!(strict_next_boundary_100ns(999_999, 30), 1_000_000); // -> b3
    assert_eq!(strict_next_boundary_100ns(1_000_000, 30), 1_333_333); // b3 -> b4
    // Second crossing (the 333_334-wide slot).
    assert_eq!(strict_next_boundary_100ns(9_666_666, 30), 10_000_000); // b29 -> b30
    assert_eq!(strict_next_boundary_100ns(9_999_999, 30), 10_000_000);
    // fps <= 0 passthrough.
    assert_eq!(strict_next_boundary_100ns(12_345, 0), 12_345);
    // Invariant: strictly greater than the input AND on the grid.
    for x in [0i64, 1, 333_333, 500_000, 9_999_999, 10_000_000, 12_333_333] {
        let n = strict_next_boundary_100ns(x, 30);
        assert!(n > x, "strict_next must advance: x={x} n={n}");
        assert_eq!(floor_boundary_100ns(n, 30), n, "on-grid: x={x} n={n}");
    }
}

// ---- B8 mirrored on the exact 100-ns grid (14 vectors) ----

#[test]
fn emit_gate_100ns_1_init_never_emits() {
    let now = b(5) + 1000;
    let (emit, next) = genlock_emit_gate_100ns(now, 0, 30, false);
    assert!(!emit);
    assert_eq!(next, strict_next_boundary_100ns(now, 30));
    assert_eq!(next, b(6));
}

#[test]
fn emit_gate_100ns_2_boundary_unmoved_before_boundary() {
    let (emit, next) = genlock_emit_gate_100ns(b(10) - 5, b(10), 30, false);
    assert!(!emit);
    assert_eq!(next, b(10));
}

#[test]
fn emit_gate_100ns_3_at_boundary_emits() {
    let (emit, next) = genlock_emit_gate_100ns(b(7), b(7), 30, false);
    assert!(emit);
    assert_eq!(next, b(8)); // 7 slots -> 8th boundary = 8 * 1e7 / 30 = 2_666_666
    assert_eq!(next, 2_666_666);
    assert_eq!(next, strict_next_boundary_100ns(b(7), 30));
}

#[test]
fn emit_gate_100ns_4_just_after_boundary_emits() {
    let (emit, next) = genlock_emit_gate_100ns(b(7) + 100, b(7), 30, false);
    assert!(emit);
    assert_eq!(next, b(8));
}

#[test]
fn emit_gate_100ns_5_zero_fps_no_panic() {
    let (emit, next) = genlock_emit_gate_100ns(123_456_789, 0, 0, false);
    assert!(!emit);
    assert_eq!(next, 0);
}

#[test]
fn emit_gate_100ns_6_zero_fps_keeps_boundary() {
    let (emit, next) = genlock_emit_gate_100ns(999, 555, 0, false);
    assert!(!emit);
    assert_eq!(next, 555);
}

#[test]
fn emit_gate_100ns_7_off_grid_nb_realigns_on_grid() {
    // The ns port keeps a misaligned nb misaligned; the exact grid REALIGNS —
    // a catch-up lands on the grid, one slot past.
    let nb = b(7) + 5; // deliberately off the exact grid
    let (emit, next) = genlock_emit_gate_100ns(nb, nb, 30, false);
    assert!(emit);
    assert!(next > nb);
    assert_eq!(floor_boundary_100ns(next, 30), next); // realigned on-grid
    assert_eq!(next, b(8));
}

#[test]
fn emit_gate_100ns_8_lag_twelve_resyncs() {
    let now = b(15) + 17;
    let (emit, next) = genlock_emit_gate_100ns(now, b(3), 30, false);
    assert!(emit);
    assert_eq!(next, strict_next_boundary_100ns(now, 30)); // resync
    assert_eq!(next, b(16));
}

#[test]
fn emit_gate_100ns_9_lag_equal_bound_catches_up() {
    // lag == 8 (the bound) still catches up ONE slot: `>` is the resync gate.
    let now = b(15) + 11;
    let (emit, next) = genlock_emit_gate_100ns(now, b(7), 30, false);
    assert!(emit);
    assert_eq!(next, b(8));
    assert_eq!(next, strict_next_boundary_100ns(b(7), 30));
}

#[test]
fn emit_gate_100ns_10_backward_step_relatches() {
    let now = b(10);
    let (emit, next) = genlock_emit_gate_100ns(now, b(100), 30, false);
    assert!(!emit);
    assert!(next <= now + interval_100ns(30));
    assert_ne!(next, b(100));
    assert_eq!(next, b(11));
}

#[test]
fn emit_gate_100ns_11_buffered_never_resyncs() {
    let now = b(18) + 11;
    let (emit, next) = genlock_emit_gate_100ns(now, b(7), 30, true);
    assert!(emit);
    assert_eq!(next, b(8)); // buffered: catch up one slot even at lag 11
}

#[test]
fn emit_gate_100ns_12_sixty_fps_decimates_to_thirty() {
    let cap = 166_666_i64; // 60 fps interval in 100-ns units
    let start = 10_000_000_i64; // second-aligned epoch
    let mut nb = 0_i64;
    let mut emitted = 0;
    for k in 0..60 {
        let now = start + k * cap;
        let (emit, next) = genlock_emit_gate_100ns(now, nb, 30, false);
        nb = next;
        if emit {
            emitted += 1;
        }
    }
    assert!((29..=31).contains(&emitted), "got {emitted}");
}

#[test]
fn emit_gate_100ns_13_buffered_drain_four_of_four() {
    let resume = b(14);
    let mut nb = b(10);
    let mut emitted = 0;
    for k in 0..4 {
        let now = resume + k;
        let (emit, next) = genlock_emit_gate_100ns(now, nb, 30, false);
        nb = next;
        if emit {
            emitted += 1;
        }
    }
    assert_eq!(emitted, 4, "every buffered frame in the drain must emit");
}

#[test]
fn emit_gate_100ns_14_buffered_drain_six_of_six_zero_skip() {
    let resume = b(110);
    let mut nb = b(100);
    let mut emitted = 0;
    let mut skipped_any = false;
    for k in 0..6 {
        let now = resume + k;
        let prev = nb;
        let (emit, next) = genlock_emit_gate_100ns(now, nb, 30, true);
        // Zero skip: each advance is exactly one grid slot from the latched
        // boundary (== the previous pending boundary, no re-latch here).
        if next != strict_next_boundary_100ns(prev, 30) {
            skipped_any = true;
        }
        nb = next;
        if emit {
            emitted += 1;
        }
    }
    assert_eq!(emitted, 6);
    assert!(!skipped_any, "buffered drain must skip no boundaries");
}

// ---- exact-grid extras: second crossing, slot-width lag, invariants ----

#[test]
fn emit_gate_100ns_crosses_a_second_boundary() {
    // Slot 29 -> slot 30 (start of the next second) is the 333_334-wide slot.
    let (emit, next) = genlock_emit_gate_100ns(b(29), b(29), 30, false);
    assert!(emit);
    assert_eq!(next, 10_000_000); // b(30) = start of the next second
    assert_eq!(next, strict_next_boundary_100ns(b(29), 30));
    // Both slot widths appear straddling the second.
    assert_eq!(b(29) - b(28), 333_333);
    assert_eq!(b(30) - b(29), 333_334);
}

#[test]
fn emit_gate_100ns_lag_counts_slots_across_a_second() {
    // 12 slots of lag straddling a second boundary (slots 25..37) -> resync.
    let now = b(37) + 5;
    let (emit, next) = genlock_emit_gate_100ns(now, b(25), 30, false);
    assert!(emit);
    assert_eq!(next, strict_next_boundary_100ns(now, 30));
    // 8 slots of lag straddling the second (slots 25..33) -> one-slot catch-up.
    let now2 = b(33) + 5;
    let (emit2, next2) = genlock_emit_gate_100ns(now2, b(25), 30, false);
    assert!(emit2);
    assert_eq!(next2, strict_next_boundary_100ns(b(25), 30));
}

#[test]
fn emit_gate_100ns_returns_on_grid_boundaries_never_stale() {
    let interval = interval_100ns(30);
    let cases: [(i64, i64, bool); 8] = [
        (b(5) + 1000, 0, false),
        (b(7), b(7), false),
        (b(15) + 17, b(3), false),
        (b(15) + 11, b(7), false),
        (b(10), b(100), false),
        (b(18) + 11, b(7), true),
        (b(14) + 2, b(10), false),
        (b(29), b(29), false),
    ];
    for (now, nb, qhf) in cases {
        let (_emit, next) = genlock_emit_gate_100ns(now, nb, 30, qhf);
        assert_eq!(
            floor_boundary_100ns(next, 30),
            next,
            "returned boundary must be on the grid: now={now} nb={nb}"
        );
        assert!(
            next > now - interval,
            "returned boundary must never be stale: now={now} next={next}"
        );
    }
}
