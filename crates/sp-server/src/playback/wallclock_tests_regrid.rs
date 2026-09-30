//! #224 part 2 (design record 5899388193): walls sharing ONE relabel registry
//! move by ONE N. WallClocks over one [`VirtualClock`] on a test's own
//! [`FleetShift`] (never the process-wide one). Pins derived with a scratch
//! Python model of the walls + the registry.
//! Wired via `#[cfg(test)] #[path = "wallclock_tests_regrid.rs"]` in
//! `wallclock.rs`.

use std::sync::Arc;

use super::*;
use crate::playback::fleet_shift::{FleetShift, WallShift, shift_100ns};

/// One 30-fps frame of virtual monotonic time (a multiple of 100 ns).
const FRAME_NS: u64 = 33_333_300;

/// A wall on `fleet` over `clk`.
fn wall_on(clk: &Arc<VirtualClock>, fleet: &Arc<FleetShift>) -> WallClock {
    WallClock::with_fleet(Box::new(clk.clone()), fleet.clone())
}

/// Advance one frame and tick every wall in `walls`, in order.
fn frame(clk: &VirtualClock, walls: &mut [&mut WallClock]) {
    clk.advance_ns(FRAME_NS);
    for w in walls.iter_mut() {
        w.tick();
    }
}

#[test]
fn a_plus_260_3_ms_step_relabels_7_slots_and_moves_the_timeline_by_r_only() {
    let clk = VirtualClock::new(0);
    let fleet = Arc::new(FleetShift::default());
    let mut wall = wall_on(&clk, &fleet);
    frame(&clk, &mut [&mut wall]);
    clk.step_utc(2_603_000);
    clk.advance_ns(FRAME_NS);
    let before = wall.now_100ns();
    wall.tick();
    assert_eq!(wall.now_100ns() - before, 269_666, "r = 26.97 ms");
    assert_eq!(wall.now_100ns(), clk.truth_100ns() - shift_100ns(7));
    assert_eq!(
        wall.shift(),
        WallShift {
            slots: 7,
            epochs: 1,
            regrids: 1,
            last_remainder_100ns: 269_666,
            last_jump_100ns: 269_666,
        }
    );
    assert_eq!((fleet.slots(), fleet.epochs()), (7, 1), "registered");
}

#[test]
fn two_walls_reading_one_step_either_side_of_a_slot_multiple_move_by_one_n() {
    // Wall B's construction read is preempted (every attempt 400 µs): its
    // UTC line is 200 µs ahead, so it reads the step 200 µs smaller. The step
    // is 7 slots + 100 µs: wall A alone splits N = 7, wall B alone N = 6.
    // Whichever confirms first registers; the other adopts that N, and both
    // end on the same timeline.
    for a_first in [true, false] {
        let clk = VirtualClock::new(0);
        let fleet = Arc::new(FleetShift::default());
        let mut a = wall_on(&clk, &fleet);
        clk.delay_next_reads(&[400_000; 8]);
        let mut b = wall_on(&clk, &fleet);
        for _ in 0..5 {
            frame(&clk, &mut [&mut a, &mut b]);
        }
        clk.step_utc(shift_100ns(7) + 1_000);
        if a_first {
            frame(&clk, &mut [&mut a, &mut b]);
        } else {
            frame(&clk, &mut [&mut b, &mut a]);
        }
        let n = if a_first { 7 } else { 6 };
        assert_eq!(fleet.slots(), n, "a_first {a_first}: one registered N");
        assert_eq!(fleet.epochs(), 1, "a_first {a_first}: one epoch");
        assert_eq!(
            (a.shift().slots, b.shift().slots),
            (n, n),
            "a_first {a_first}"
        );
        // B (the smaller reading) adopting 7 is left 100 µs short: ONE hold
        // of that residue, never a step back.
        let b_holds = u64::from(a_first);
        assert_eq!(
            b.anchor_stats().holds_followed,
            b_holds,
            "a_first {a_first}"
        );
        clk.advance_ns(1_000_000);
        assert_eq!(
            b.now_100ns(),
            a.now_100ns(),
            "a_first {a_first}: one timeline"
        );
        assert_eq!(
            a.now_100ns(),
            clk.truth_100ns() - shift_100ns(n),
            "a_first {a_first}"
        );
    }
}

#[test]
fn a_wall_two_epochs_behind_adopts_both_and_a_wall_built_after_them_starts_at_the_current_k() {
    let clk = VirtualClock::new(0);
    let fleet = Arc::new(FleetShift::default());
    let mut a = wall_on(&clk, &fleet);
    let mut b = wall_on(&clk, &fleet);
    for _ in 0..5 {
        frame(&clk, &mut [&mut a, &mut b]);
    }
    // Two steps only wall A ticks through: +260.3 ms (N = 7), then −19.8 ms
    // (N = −1 at K = 7: r = 135 334).
    clk.step_utc(2_603_000);
    for _ in 0..4 {
        frame(&clk, &mut [&mut a]);
    }
    clk.step_utc(-198_000);
    frame(&clk, &mut [&mut a]);
    assert_eq!(a.shift().slots, 6);
    assert_eq!(a.shift().last_remainder_100ns, 135_334);
    assert_eq!(fleet.epochs(), 2);
    // Wall B reads both as one +240.5 ms step and adopts ΣN = 6 (its own
    // remainder, 40.5 ms, is past one slot: the epochs' floors each kept a
    // part), so it lands on A's timeline.
    b.tick();
    assert_eq!(
        (
            b.shift().slots,
            b.shift().epochs,
            b.shift().last_remainder_100ns
        ),
        (6, 2, 405_000)
    );
    assert_eq!(b.now_100ns(), a.now_100ns(), "one timeline");
    assert_eq!(a.now_100ns(), clk.truth_100ns() - shift_100ns(6));
    // A wall built now starts at K = 6 with both epochs applied.
    let c = wall_on(&clk, &fleet);
    assert_eq!((c.shift().slots, c.shift().epochs), (6, 2));
    assert_eq!(c.now_100ns(), a.now_100ns());
    assert_eq!(c.shift().regrids, 0, "it followed nothing itself");
}

#[test]
fn a_wall_on_its_own_registry_never_moves_another_registry() {
    // Tests build their own registry: a step followed on one never reaches
    // another (and never the process-wide one a production wall reads).
    let clk = VirtualClock::new(0);
    let mine = Arc::new(FleetShift::default());
    let other = Arc::new(FleetShift::default());
    let mut wall = wall_on(&clk, &mine);
    clk.step_utc(900_000);
    frame(&clk, &mut [&mut wall]);
    assert_eq!(mine.slots(), 2);
    assert_eq!((other.slots(), other.epochs()), (0, 0));
    assert!(Arc::ptr_eq(wall.fleet(), &mine));
    // `WallClock::new` builds a registry of its own.
    let fresh = WallClock::new(Box::new(clk.clone()));
    assert!(!Arc::ptr_eq(fresh.fleet(), &mine));
    assert_eq!(fresh.fleet().slots(), 0);
}

#[test]
fn a_hold_is_recorded_only_when_the_timeline_moved_back_never_by_the_step_direction() {
    // #224 part 2: a relabelled backward step moves the timeline FORWARD by
    // r (or not at all): no hold. Only a negative timeline movement (a ≤ 1 ms
    // residue) is one.
    let mut st = WallAnchorStats::default();
    let backward = |total_100ns| FollowedStep {
        total_100ns,
        direction: StepDirection::Backward,
    };
    let moved = |applied_100ns| AnchorStep {
        applied_100ns,
        carry_100ns: 0,
    };
    st.record_follow(&backward(-198_000), &moved(135_333));
    st.record_follow(&backward(-15_000_000), &moved(0));
    assert_eq!(
        (
            st.steps_followed,
            st.holds_followed,
            st.last_hold_us,
            st.last_step_us
        ),
        (2, 0, 0, -1_500_000)
    );
    let forward = FollowedStep {
        total_100ns: 2_332_334,
        direction: StepDirection::Forward,
    };
    st.record_follow(&forward, &moved(-1_000));
    assert_eq!((st.holds_followed, st.last_hold_us), (1, 100));
}
