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

#[test]
fn max_catchup_matches_contract() {
    assert_eq!(GENLOCK_MAX_CATCHUP_INTERVALS, 8);
}

// ---- B3: next_boundary_100ns (the CEIL twin — sleep target only) ----

#[test]
fn next_boundary_100ns_b3_vectors() {
    assert_eq!(next_boundary_100ns(0, 30), 333_333);
    assert_eq!(next_boundary_100ns(1, 60), 166_666);
    assert_eq!(next_boundary_100ns(9_900_000, 60), 10_000_000);
    assert_eq!(next_boundary_100ns(12_345, 0), 12_345); // fps <= 0 passthrough
}

// ===========================================================================
// Exact-100-ns-grid emission (#147 rework) — the twin the `Pacer` actually
// uses (SongPlayer generates its own timing, so pacing + stamps share ONE
// grid). camera-box's 14 B8 scenarios are mirrored on the exact grid with values
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
    let cases: [(i64, i64, bool); 9] = [
        (b(5) + 1000, 0, false),
        (b(7), b(7), false),
        (b(15) + 17, b(3), false),
        (b(15) + 11, b(7), false),
        (b(10), b(100), false),
        (b(18) + 11, b(7), true),
        (b(14) + 2, b(10), false),
        (b(29), b(29), false),
        // A WAIT whose pending boundary is one 333_334-WIDE slot ahead
        // (b(2)=666_666 -> b(3)=1_000_000): `next - now == 333_334 > interval`.
        // The pre-fix `next <= now + interval` bound was off by one here.
        (b(2), 0, false),
    ];
    for (now, nb, qhf) in cases {
        let (emit, next) = genlock_emit_gate_100ns(now, nb, 30, qhf);
        // Every returned boundary is on the exact grid and strictly positive.
        assert_eq!(
            floor_boundary_100ns(next, 30),
            next,
            "returned boundary must be on the grid: now={now} nb={nb}"
        );
        assert!(next > 0, "boundary must be positive: now={now} next={next}");

        let serviced = if nb == 0 || nb > floor_boundary_100ns(now, 30) + interval {
            strict_next_boundary_100ns(now, 30)
        } else {
            nb
        };
        if emit {
            // An emit ADVANCES strictly past the serviced boundary. The
            // catch-up target is intentionally BEHIND `now` (a transient
            // one-slot advance the pacer immediately re-services), so
            // `> now - interval` does NOT hold there — only `> serviced` does.
            assert!(
                next > serviced,
                "emit must advance past the serviced boundary: now={now} next={next}"
            );
        } else {
            // A WAIT returns the IMMEDIATE pending boundary: strictly after
            // `now`, never more than one grid SLOT ahead — never stale. Compare
            // against the exact next boundary, not the nominal `interval`: ten
            // slots per second are 333_334 wide, so `now + interval` was off by
            // one on those (fix-lane-2 off-by-one).
            assert!(
                now < next && next <= strict_next_boundary_100ns(now, 30),
                "pending boundary must be the immediate next: now={now} next={next}"
            );
        }
    }
}
