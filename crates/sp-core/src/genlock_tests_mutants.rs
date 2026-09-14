//! Mutation-kill tests for `sp_core::genlock` (PR #153 / 0.47.0 release).
//!
//! Each test targets a specific surviving mutant reported by the PR mutation
//! run: it pins an EXACT integer / tuple at the precise input (boundary
//! equality, large timestamp exercising the second-anchor, `/` vs `%`/`*`,
//! `-` vs `+`/`/`) where the mutated operator diverges from the original.
//! Wired via `#[cfg(test)] #[path = "genlock_tests_mutants.rs"] mod …;` from
//! `genlock.rs`, so `super::*` is the genlock module under test.

use super::*;

// ---------------------------------------------------------------------------
// floor_boundary_100ns 58:45 `*` -> `/` (second-anchor `cs`)
//
// Every existing vector uses `now < 1e7`, where `now / 1e7 == 0` so `* 1e7`
// and `/ 1e7` both yield `cs == 0` — indistinguishable. A realistic post-2020
// wall-clock reading makes `now / 1e7` large, so `* 1e7` recovers the
// containing-second start while `/ 1e7` collapses it to a tiny number and the
// floor lands on the wrong grid slot.
// ---------------------------------------------------------------------------

#[test]
fn floor_boundary_large_timestamp_pins_second_anchor() {
    // 1.6e9 s past the epoch (~2020-09-13) + 5 ticks (500 ns) into the second.
    // floor is the exact 1.6e9-second grid boundary at 16_000_000_000_000_000.
    let now = 16_000_000_000_000_005i64;
    assert_eq!(
        floor_boundary_100ns(now, 30),
        16_000_000_000_000_000,
        "floor must anchor to the containing second (cs uses `* UNITS`, not `/`)"
    );
}

// ---------------------------------------------------------------------------
// genlock_emit_gate_100ns 245:77 `>` -> `>=` (re-latch guard)
//
// At `next_boundary_in == floor_now + interval` exactly, `>` keeps the pending
// boundary; `>=` wrongly re-latches to the next grid boundary after `now`.
// floor_boundary_100ns(700_000, 30) == 666_666; interval_100ns(30) == 333_333;
// 666_666 + 333_333 == 999_999.
// ---------------------------------------------------------------------------

#[test]
fn emit_gate_100ns_keeps_pending_boundary_at_equality_point() {
    // next_boundary_in == floor_now + interval exactly -> keep it, do not emit.
    assert_eq!(
        genlock_emit_gate_100ns(700_000, 999_999, 30, false),
        (false, 999_999),
        "at the exact re-latch equality point `>` keeps the pending boundary"
    );
}

// ---------------------------------------------------------------------------
// genlock_latched_boundary 271:50 `>` -> `>=` (ns re-latch guard), exercised
// through the public `genlock_emit_gate`.
//
// interval_ns(30) == 33_333_333. now_ns == 50_000_000, next_boundary_ns ==
// now_ns + interval_ns == 83_333_333 exactly. `>` keeps 83_333_333; `>=`
// re-latches to now - now%interval + interval == 66_666_666.
// ---------------------------------------------------------------------------

#[test]
fn latched_boundary_keeps_pending_at_equality_point_via_emit_gate() {
    assert_eq!(
        genlock_emit_gate(50_000_000, 83_333_333, 33_333_333, false),
        (false, 83_333_333),
        "at next_boundary == now + interval exactly, `>` keeps the pending boundary"
    );
}

// ---------------------------------------------------------------------------
// lag_slots_100ns — 385:5 body -> 0/1/-1, 386:17 `==`->`!=`, 386:22 `||`->`&&`,
// 386:41 `<=`->`>`, 389:40 `/`->`%`/`*`, 389:22 `-`->`+`/`/`.
// signature: lag_slots_100ns(boundary_100ns, floor_now_100ns, fps).
// interval_100ns(30) == 333_333.
// ---------------------------------------------------------------------------

#[test]
fn lag_slots_guard_returns_zero_when_floor_at_or_before_boundary() {
    // floor_now (500_000) < boundary (1_000_000): guard fires -> 0.
    // Kills body->1, body->-1, `||`->`&&` (would compute -1), `<=`->`>` (-1).
    assert_eq!(lag_slots_100ns(1_000_000, 500_000, 30), 0);
}

#[test]
fn lag_slots_five_from_zero_boundary() {
    // boundary 0, floor_now 1_700_000: 1_700_000 / 333_333 == 5.
    // Kills body->0, `==`->`!=` (would guard-return 0), `/`->`%` (== 33_335),
    // `/`->`*` (huge).
    assert_eq!(lag_slots_100ns(0, 1_700_000, 30), 5);
}

#[test]
fn lag_slots_five_from_nonzero_boundary_pins_subtraction() {
    // boundary 333_333, floor_now 1_999_998: (1_999_998 - 333_333)/333_333 == 5.
    // Kills `-`->`+` ((1_999_998+333_333)/333_333 == 7) and
    // `-`->`/` ((1_999_998/333_333)/333_333 == 6/333_333 == 0).
    assert_eq!(lag_slots_100ns(333_333, 1_999_998, 30), 5);
}
