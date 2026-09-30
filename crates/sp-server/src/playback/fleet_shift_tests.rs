//! #224 part 2 (design record 5899388193): the pure relabel split and the
//! registry. Every pin was derived with a scratch Python model of the split
//! (P = 10⁷/30, D(K) = ⌈K·P⌉, N = ⌊S/P⌋, r = S − (D(K+N) − D(K))).
//! Wired via `#[cfg(test)] #[path = "fleet_shift_tests.rs"] mod tests;`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::*;
use crate::playback::wallclock::{Anchor, AnchorSample, WALL_REJOIN_IDLE, utc_now_100ns};
use sp_core::genlock::{grid_boundary_100ns, grid_index_100ns};

/// 1 ms in 100-ns units.
const MS: i64 = 10_000;
/// One whole slot rounded up (the widest grid slot), 100 ns.
const SLOT_CEIL: i64 = 333_334;

#[test]
fn the_design_table_splits_each_step_into_whole_slots_and_a_small_forward_remainder() {
    // (S in 100 ns, N, r in 100 ns) — the design record's table at K = 0.
    let table = [
        (2_603_000, 7, 269_666), // +260.3 ms → 7 / 26.97 ms
        (2_190_300, 6, 190_300), // +219.03 ms → 6 / 19.03 ms
        (900_000, 2, 233_333),   // +90 ms → 2 / 23.3 ms
        (500_000, 1, 166_666),   // +50 ms → 1 / 16.7 ms
        (-198_000, -1, 135_333), // −19.8 ms → −1 / 13.53 ms
        (-510_390, -2, 156_276), // −51.039 ms → −2 / 15.63 ms
        (-15_000_000, -45, 0),   // −1.5 s → −45 / 0
    ];
    for (step, slots, remainder) in table {
        assert_eq!(
            split(step, 0),
            Regrid {
                slots,
                remainder_100ns: remainder
            },
            "S = {step}"
        );
        assert_eq!(whole_slots(step), slots, "S = {step}");
    }
}

#[test]
fn the_shift_rounds_up_so_a_whole_slot_count_is_exact_at_every_multiple_of_three() {
    let pins = [
        (0, 0),
        (1, 333_334),
        (2, 666_667),
        (3, 1_000_000),
        (7, 2_333_334),
        (30, 10_000_000),
        (-1, -333_333),
        (-2, -666_666),
        (-3, -1_000_000),
        (-45, -15_000_000),
    ];
    for (k, d) in pins {
        assert_eq!(shift_100ns(k), d, "D({k})");
    }
    assert_eq!(relabel_100ns(0, 7), 2_333_334);
    assert_eq!(relabel_100ns(1, 1), 333_333, "D(2) − D(1)");
    assert_eq!(relabel_100ns(2, -2), -666_667, "D(0) − D(2)");
}

#[test]
fn a_step_one_unit_either_side_of_a_slot_multiple_changes_n_and_r_exactly() {
    // The k·P ± 1 knife-edges: D(k) − 1 is ⌊k·P⌋ (the slot not reached), D(k)
    // is the first 100 ns of k whole slots.
    for k in [1i64, 2, 3, 7, 30, -1, -2, -30, -45] {
        let d = shift_100ns(k);
        let below = split(d - 1, 0);
        assert_eq!(below.slots, k - 1, "k = {k}: one unit under");
        assert_eq!(
            below.remainder_100ns,
            d - 1 - shift_100ns(k - 1),
            "k = {k}: one unit under"
        );
        assert_eq!(
            split(d, 0),
            Regrid {
                slots: k,
                remainder_100ns: 0
            },
            "k = {k}: on the multiple"
        );
        assert_eq!(
            split(d + 1, 0),
            Regrid {
                slots: k,
                remainder_100ns: 1
            },
            "k = {k}: one unit over"
        );
    }
    // Exact numbers for the first ones (the model's knife-edge table).
    assert_eq!(split(333_333, 0).remainder_100ns, 333_333);
    assert_eq!(split(-333_334, 0).remainder_100ns, 333_332);
    assert_eq!(split(-15_000_001, 0).slots, -46);
}

#[test]
fn the_remainder_is_never_negative_and_at_most_one_slot_for_any_step_and_any_k() {
    // A dense sweep: every wall starts at some K after earlier steps, and the
    // remainder must stay a small FORWARD movement of its timeline.
    for k in -60i64..=60 {
        for step in (-12_000_000i64..=12_000_000).step_by(83_329) {
            let r = split(step, k);
            assert!(
                (0..=SLOT_CEIL).contains(&r.remainder_100ns),
                "S = {step}, K = {k}: r = {}",
                r.remainder_100ns
            );
            assert_eq!(r.slots, whole_slots(step), "N does not depend on K");
        }
    }
    // The ceiling's carry: at K = 1 the remainder of a 333 334 step is 1.
    assert_eq!(
        split(333_334, 1),
        Regrid {
            slots: 1,
            remainder_100ns: 1
        }
    );
}

#[test]
fn the_wire_stamp_of_an_on_grid_boundary_is_exactly_the_boundary_k_slots_later() {
    let base = grid_index_100ns(17_900_000_000_000_000, 30);
    for n in 0..95i64 {
        let b = grid_boundary_100ns(base + n, 30);
        for k in -50i64..=50 {
            assert_eq!(
                wire_stamp_100ns(b, k),
                grid_boundary_100ns(base + n + k, 30),
                "n = {n}, K = {k}"
            );
        }
    }
    // An off-grid instant is shifted, then floored (never future-dated): one
    // unit before boundary 1, shifted by D(1) = 333 334, is boundary 2.
    let b = grid_boundary_100ns(base, 30);
    assert_eq!(wire_stamp_100ns(b + 100_000, 0), b);
    assert_eq!(
        wire_stamp_100ns(b + 333_332, 1),
        grid_boundary_100ns(base + 2, 30)
    );
}

fn epoch(step_100ns: i64, slots: i64) -> Epoch {
    Epoch { step_100ns, slots }
}

#[test]
fn a_step_no_epoch_covers_is_registered_with_its_own_split() {
    assert_eq!(
        adopt(&[], 2_603_000),
        Adoption {
            slots: 7,
            consumed: 0,
            new_epoch: Some(epoch(2_603_000, 7))
        }
    );
    assert_eq!(
        adopt(&[], -198_000),
        Adoption {
            slots: -1,
            consumed: 0,
            new_epoch: Some(epoch(-198_000, -1))
        }
    );
}

#[test]
fn a_wall_reading_the_same_step_within_3_ms_adopts_its_n_even_across_a_slot_multiple() {
    // The first wall read 2 333 334 (N = 7, r = 0); a second wall reads the
    // same step one unit smaller, on the other side of the 7-slot multiple:
    // alone it would split N = 6, but it adopts 7. Up to 3 ms the difference
    // is the two walls' line errors (review r1 🟡 D: a lone-outlier resample
    // leaves a line ~1.3 ms off; two walls with opposite ones, 2.6 ms).
    assert_eq!(STEP_RESIDUE_100NS, 3 * MS);
    let first = [epoch(2_333_334, 7)];
    for step in [
        2_333_333,
        2_333_334 - MS,
        2_333_334 + MS,
        2_333_334 - 13_000,
        2_333_334 - 26_000,
        2_333_334 - 3 * MS,
        2_333_334 + 3 * MS,
    ] {
        assert_eq!(
            adopt(&first, step),
            Adoption {
                slots: 7,
                consumed: 1,
                new_epoch: None
            },
            "S = {step}"
        );
    }
    // Just over 3 ms away: another step, registered on top.
    assert_eq!(
        adopt(&first, 2_333_334 + 3 * MS + 1),
        Adoption {
            slots: 7,
            consumed: 1,
            new_epoch: Some(epoch(3 * MS + 1, 0))
        }
    );
    assert_eq!(
        adopt(&first, 2_333_334 - 3 * MS - 1),
        Adoption {
            slots: 6,
            consumed: 1,
            new_epoch: Some(epoch(-3 * MS - 1, -1))
        }
    );
}

#[test]
fn a_step_within_3_ms_of_the_closest_run_is_a_walls_own_residue_never_an_epoch() {
    let none = Adoption {
        slots: 0,
        consumed: 0,
        new_epoch: None,
    };
    // Nothing registered: a step of at most 3 ms is applied by each wall on
    // its own (N = 0). Registered, −2.5 ms would be N = −1 and move every
    // wall's labels a slot back.
    assert_eq!(adopt(&[], 3 * MS), none);
    assert_eq!(adopt(&[], -3 * MS), none);
    assert_eq!(adopt(&[], -25_000), none);
    assert_eq!(
        adopt(&[], 3 * MS + 1),
        Adoption {
            slots: 0,
            consumed: 0,
            new_epoch: Some(epoch(3 * MS + 1, 0))
        }
    );
    assert_eq!(
        adopt(&[], -3 * MS - 1),
        Adoption {
            slots: -1,
            consumed: 0,
            new_epoch: Some(epoch(-3 * MS - 1, -1))
        }
    );
    // Behind two epochs, the CLOSEST run wins: 1.3 ms under the first.
    let epochs = [epoch(500_000, 1), epoch(-198_000, -1)];
    assert_eq!(
        adopt(&epochs, 487_000),
        Adoption {
            slots: 1,
            consumed: 1,
            new_epoch: None
        }
    );
    // 1.5 ms over both: ΣN = 0, both consumed.
    assert_eq!(
        adopt(&epochs, 317_000),
        Adoption {
            slots: 0,
            consumed: 2,
            new_epoch: None
        }
    );
    // A tie between two runs: the shorter wins (no epoch is ever that
    // small; the rule is total all the same).
    assert_eq!(adopt(&[epoch(30_000, 0)], 15_000), none);
}

#[test]
fn a_wall_two_epochs_behind_adopts_both_and_one_epoch_behind_the_first_only() {
    let epochs = [epoch(500_000, 1), epoch(-198_000, -1), epoch(900_000, 2)];
    // It saw only the first step (it read the clock before the second).
    assert_eq!(
        adopt(&epochs, 500_000),
        Adoption {
            slots: 1,
            consumed: 1,
            new_epoch: None
        }
    );
    // It saw the first two as one step: ΣS = 302 000, ΣN = 0.
    assert_eq!(
        adopt(&epochs, 302_000),
        Adoption {
            slots: 0,
            consumed: 2,
            new_epoch: None
        }
    );
    // All three: ΣS = 1 202 000, ΣN = 2.
    assert_eq!(
        adopt(&epochs, 1_202_000),
        Adoption {
            slots: 2,
            consumed: 3,
            new_epoch: None
        }
    );
    // All three and a new step of +20 ms on top: registered as its own rest.
    assert_eq!(
        adopt(&epochs, 1_402_000),
        Adoption {
            slots: 2,
            consumed: 3,
            new_epoch: Some(epoch(200_000, 0))
        }
    );
}

#[test]
fn the_registry_registers_once_and_every_other_wall_adopts_the_same_n() {
    let fleet = FleetShift::default();
    assert_eq!((fleet.slots(), fleet.epochs()), (0, 0));
    assert_eq!(fleet.current(), WallShift::default());
    // Wall A confirms +260.3 ms first.
    assert_eq!(
        fleet.follow(0, 2_603_000),
        Relabel {
            slots: 7,
            epochs: 1
        }
    );
    assert_eq!((fleet.slots(), fleet.epochs()), (7, 1));
    // Wall B reads it 200 µs smaller and adopts: nothing new registered.
    assert_eq!(
        fleet.follow(0, 2_601_000),
        Relabel {
            slots: 7,
            epochs: 1
        }
    );
    assert_eq!((fleet.slots(), fleet.epochs()), (7, 1));
    // Wall C reads it 1.3 ms smaller (its line ~1.3 ms off), wall D 2.6 ms
    // smaller (its line off the other way from A's): both adopt.
    for reading in [2_590_000, 2_577_000] {
        assert_eq!(
            fleet.follow(0, reading),
            Relabel {
                slots: 7,
                epochs: 1
            },
            "{reading}"
        );
    }
    assert_eq!((fleet.slots(), fleet.epochs()), (7, 1));
    // A wall built now starts at K = 7 with the epoch applied.
    assert_eq!(
        fleet.current(),
        WallShift {
            slots: 7,
            epochs: 1,
            ..WallShift::default()
        }
    );
    // Wall A, at 1 epoch applied, confirms −19.8 ms: a second epoch.
    assert_eq!(
        fleet.follow(1, -198_000),
        Relabel {
            slots: -1,
            epochs: 2
        }
    );
    assert_eq!((fleet.slots(), fleet.epochs()), (6, 2));
    // A wall still at 0 epochs sees both as one +240.5 ms step: ΣN = 6.
    assert_eq!(
        fleet.follow(0, 2_405_000),
        Relabel {
            slots: 6,
            epochs: 2
        }
    );
    // An applied count past the end never panics: a new step is registered.
    assert_eq!(
        fleet.follow(9, 500_000),
        Relabel {
            slots: 1,
            epochs: 3
        }
    );
    assert_eq!((fleet.slots(), fleet.epochs()), (7, 3));
}

#[test]
fn a_joining_wall_copies_the_line_published_within_10_s_else_starts_on_its_sample() {
    let fleet = FleetShift::default();
    let t0 = Instant::now();
    let sample = |instant| AnchorSample {
        instant,
        utc_100ns: 5_000,
        bracket: Duration::ZERO,
    };
    let _ = fleet.follow(0, 2_603_000); // K = 7, one epoch
    // Nothing published: the sample itself, at the current K.
    let (anchor, shift) = fleet.join(&sample(t0));
    assert_eq!(
        anchor,
        Anchor {
            instant: t0,
            utc_100ns: 5_000
        }
    );
    assert_eq!(
        shift,
        WallShift {
            slots: 7,
            epochs: 1,
            ..WallShift::default()
        }
    );
    // A wall published its line: a pre-step one (K = 0, no epoch applied).
    let line = FleetLine {
        at: t0,
        anchor: Anchor {
            instant: t0,
            utc_100ns: 1_000,
        },
        slots: 0,
        epochs: 0,
    };
    fleet.publish(line);
    for after in [Duration::ZERO, WALL_REJOIN_IDLE] {
        let (anchor, shift) = fleet.join(&sample(t0 + after));
        assert_eq!(anchor, line.anchor, "{after:?}: its line");
        assert_eq!(shift, WallShift::default(), "{after:?}: its K and epochs");
    }
    // Over 10 s after it: stale, the sample at the current K.
    let late = t0 + WALL_REJOIN_IDLE + Duration::from_nanos(100);
    let (anchor, shift) = fleet.join(&sample(late));
    assert_eq!(
        anchor,
        Anchor {
            instant: late,
            utc_100ns: 5_000
        }
    );
    assert_eq!((shift.slots, shift.epochs), (7, 1));
    // The newest publish wins.
    let newer = FleetLine {
        at: late,
        anchor: Anchor {
            instant: late,
            utc_100ns: 9_000,
        },
        slots: 7,
        epochs: 1,
    };
    fleet.publish(newer);
    assert_eq!(
        fleet.join(&sample(late)),
        (
            newer.anchor,
            WallShift {
                slots: 7,
                epochs: 1,
                ..WallShift::default()
            }
        ),
        "its line WITH its K and epochs"
    );
}

#[test]
fn the_timeline_the_wire_and_the_label_read_the_registry_shift() {
    let fleet = FleetShift::default();
    // A 10 s step: K = 300, D(300) = 10 s.
    assert_eq!(fleet.follow(0, 100_000_000).slots, 300);
    let expected = utc_now_100ns() - 100_000_000;
    let timeline = fleet.timeline_now_100ns();
    assert!(
        (timeline - expected).abs() < 10_000_000,
        "the timeline is realtime − D(K): {timeline} vs {expected}"
    );
    let b = grid_boundary_100ns(grid_index_100ns(17_900_000_000_000_000, 30), 30);
    assert_eq!(fleet.wire_100ns(b), b + 100_000_000);
    assert_eq!(fleet.label_100ns(b + 7), b + 100_000_007);
}

#[test]
fn the_process_wide_registry_is_one_and_the_free_readers_use_it() {
    assert!(Arc::ptr_eq(global(), global()), "one registry per process");
    // Tests never register into the global registry (a test builds its own),
    // so its K is whatever the process has — read it, never assume 0.
    let k = global().slots();
    let b = grid_boundary_100ns(grid_index_100ns(17_900_000_000_000_000, 30), 30);
    assert_eq!(wire_100ns(b + 1_234), wire_stamp_100ns(b + 1_234, k));
    assert_eq!(label_100ns(b + 1_234), b + 1_234 + shift_100ns(k));
    let expected = utc_now_100ns() - shift_100ns(k);
    assert!((timeline_now_100ns() - expected).abs() < 10_000_000);
}
