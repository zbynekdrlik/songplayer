//! #215: the exact grid index ↔ boundary helpers (`grid_index_100ns` /
//! `grid_boundary_100ns`) the program transition window counts its
//! boundaries with. Exact values at a real 2026 wall reading, so every
//! arithmetic mutant dies; the round trip crosses second edges, where the
//! 30 fps slots are 333_334 wide instead of 333_333.
//! Wired via `#[cfg(test)] #[path = "genlock_tests_grid_index.rs"] mod …;` from
//! `genlock.rs`, so `super::*` is the genlock module under test.

use super::*;

/// 2026-09 in 100 ns since the epoch — exactly on a second (slot 0).
const T0: i64 = 17_900_000_000_000_000;

#[test]
fn a_boundary_indexes_as_second_times_fps_plus_its_slot() {
    assert_eq!(grid_index_100ns(T0, 30), 53_700_000_000);
    assert_eq!(grid_index_100ns(T0 + 333_333, 30), 53_700_000_001);
    assert_eq!(
        grid_index_100ns(T0 + 666_666, 30),
        53_700_000_002,
        "slot 2 starts 666_666 in (2/30 s, floored)"
    );
    assert_eq!(grid_index_100ns(T0 + 9_666_666, 30), 53_700_000_029);
    assert_eq!(grid_index_100ns(T0 + 10_000_000, 30), 53_700_000_030);
    assert_eq!(grid_index_100ns(T0 + 166_666, 60), 107_400_000_001);
}

#[test]
fn an_index_maps_back_to_its_boundary() {
    assert_eq!(grid_boundary_100ns(53_700_000_000, 30), T0);
    assert_eq!(grid_boundary_100ns(53_700_000_001, 30), T0 + 333_333);
    assert_eq!(grid_boundary_100ns(53_700_000_029, 30), T0 + 9_666_666);
    assert_eq!(grid_boundary_100ns(53_700_000_030, 30), T0 + 10_000_000);
    assert_eq!(grid_boundary_100ns(53_700_000_031, 30), T0 + 10_333_333);
    assert_eq!(grid_boundary_100ns(107_400_000_001, 60), T0 + 166_666);
}

#[test]
fn indices_round_trip_every_boundary_across_second_edges() {
    let mut b = floor_boundary_100ns(T0 + 9_000_000, 30);
    let first = grid_index_100ns(b, 30);
    assert_eq!(first, 53_700_000_027);
    for k in 0..70 {
        assert_eq!(grid_index_100ns(b, 30), first + k, "index of boundary {k}");
        assert_eq!(grid_boundary_100ns(first + k, 30), b, "boundary {k}");
        b = strict_next_boundary_100ns(b, 30);
    }
}

#[test]
fn a_non_positive_fps_indexes_nothing() {
    assert_eq!(grid_index_100ns(T0 + 333_333, 0), 0);
    assert_eq!(grid_index_100ns(T0 + 333_333, -30), 0);
    assert_eq!(grid_boundary_100ns(53_700_000_001, 0), 0);
    assert_eq!(grid_boundary_100ns(53_700_000_001, -30), 0);
}
