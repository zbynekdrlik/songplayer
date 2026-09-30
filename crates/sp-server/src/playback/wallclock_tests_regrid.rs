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
            moved_100ns: 269_666,
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
fn a_wall_built_between_a_step_and_its_registration_follows_the_step_itself() {
    // Review r1: a wall built right after a date step, before any wall
    // registered it, JOINS the line the busy wall published at its last tick
    // (the pre-step line), so it reads the step itself at its first tick and
    // adopts the one N, on the one timeline. Anchored on the stepped UTC at
    // K = 0 instead, it would sit S − r off every other wall for good.
    for (step, n) in [(2_603_000, 7), (-15_000_000, -45)] {
        let clk = VirtualClock::new(0);
        let fleet = Arc::new(FleetShift::default());
        let mut busy = wall_on(&clk, &fleet);
        for _ in 0..5 {
            frame(&clk, &mut [&mut busy]);
        }
        clk.step_utc(step);
        let mut late = wall_on(&clk, &fleet);
        frame(&clk, &mut [&mut busy, &mut late]);
        assert_eq!((fleet.slots(), fleet.epochs()), (n, 1), "{step}: one epoch");
        assert_eq!(late.shift().slots, n, "{step}");
        assert_eq!(late.now_100ns(), busy.now_100ns(), "{step}: one timeline");
    }
}

#[test]
fn a_wall_idle_for_20_min_rejoins_the_fleets_line_and_registers_nothing() {
    // Review r1 🔴 C: the legacy submit wall ticks only per submitted frame.
    // Idle 20 min at ±30 ppm its line drifted ±36 ms from UTC; read as a
    // date step, registering it would relabel every paced sender, whose
    // timelines never moved. It rejoins the busy wall's line instead: a jump
    // ahead, or ONE hold, no epoch. With a +260.3 ms step during the gap it
    // lands on K = 7 too. (step, ppm, the rejoin's movement: pins from a
    // scratch Python model of the walls + the registry.)
    for (step, ppm, jump, k) in [
        (0, 30, 359_999, 0),
        (0, -30, -360_000, 0),
        (2_603_000, 30, 629_685, 7),
        (2_603_000, -30, -90_354, 7),
    ] {
        let clk = VirtualClock::new(ppm);
        let fleet = Arc::new(FleetShift::default());
        let mut busy = wall_on(&clk, &fleet);
        let mut idle = wall_on(&clk, &fleet);
        frame(&clk, &mut [&mut busy, &mut idle]);
        for i in 0..36_000 {
            if i == 18_000 {
                clk.step_utc(step);
            }
            frame(&clk, &mut [&mut busy]);
        }
        frame(&clk, &mut [&mut busy, &mut idle]);
        let epochs = usize::from(step != 0);
        assert_eq!(
            (fleet.slots(), fleet.epochs()),
            (k, epochs),
            "{step} {ppm}: only the busy wall's step is an epoch"
        );
        let shift = idle.shift();
        assert_eq!(
            (
                shift.slots,
                shift.epochs,
                shift.regrids,
                shift.last_jump_100ns
            ),
            (k, epochs, 1, jump),
            "{step} {ppm}"
        );
        assert_eq!(idle.anchor_stats().steps_followed, 0, "{step} {ppm}");
        // Past the hold (up to 36 ms): on the busy wall's line.
        clk.advance_ns(40_000_000);
        assert_eq!(idle.now_100ns(), busy.now_100ns(), "{step} {ppm}");
    }
    // Alone (no wall kept ticking): it rejoins the realtime clock itself.
    let clk = VirtualClock::new(30);
    let fleet = Arc::new(FleetShift::default());
    let mut alone = wall_on(&clk, &fleet);
    frame(&clk, &mut [&mut alone]);
    clk.advance_ns(1_200_000_000_000);
    alone.tick();
    assert_eq!((fleet.slots(), fleet.epochs()), (0, 0));
    assert_eq!(alone.now_100ns(), clk.truth_100ns());
}

#[test]
fn a_relabelled_wall_idle_for_20_min_rejoins_on_the_same_k() {
    // Both walls followed +260.3 ms (K = 7) before one went idle: the rejoin
    // moves its timeline by its drift only (its own and the joined line both
    // sit D(7) behind their labels), K stays 7, nothing registered.
    for (ppm, jump) in [(30, 360_000), (-30, -360_000)] {
        let clk = VirtualClock::new(ppm);
        let fleet = Arc::new(FleetShift::default());
        let mut busy = wall_on(&clk, &fleet);
        let mut idle = wall_on(&clk, &fleet);
        for f in 0..10 {
            if f == 5 {
                clk.step_utc(2_603_000);
            }
            frame(&clk, &mut [&mut busy, &mut idle]);
        }
        assert_eq!((idle.shift().slots, idle.shift().regrids), (7, 1), "{ppm}");
        for _ in 0..36_000 {
            frame(&clk, &mut [&mut busy]);
        }
        frame(&clk, &mut [&mut busy, &mut idle]);
        assert_eq!((fleet.slots(), fleet.epochs()), (7, 1), "{ppm}");
        let shift = idle.shift();
        assert_eq!(
            (
                shift.slots,
                shift.epochs,
                shift.regrids,
                shift.last_jump_100ns
            ),
            (7, 1, 2, jump),
            "{ppm}"
        );
        clk.advance_ns(40_000_000);
        assert_eq!(idle.now_100ns(), busy.now_100ns(), "{ppm}");
    }
}

#[test]
fn a_rejoin_on_the_resample_tick_never_reads_its_own_hold_as_a_step() {
    // Review r2 🔴 B: the idle wall was 99 ticks into its resample count, so
    // its rejoin tick is the 100th. Measured against the frozen wall, the
    // resample read the rejoin's own 36 ms hold as a backward step, cut it to
    // 1 ms and the probe then registered it (N = −2 for everyone). A rejoin
    // restarts the count (a fresh anchor), and no resample runs inside a hold.
    let clk = VirtualClock::new(-30);
    let fleet = Arc::new(FleetShift::default());
    let mut busy = wall_on(&clk, &fleet);
    let mut idle = wall_on(&clk, &fleet);
    for _ in 0..99 {
        frame(&clk, &mut [&mut busy, &mut idle]);
    }
    for _ in 0..36_000 {
        frame(&clk, &mut [&mut busy]);
    }
    frame(&clk, &mut [&mut busy, &mut idle]);
    assert_eq!(
        (fleet.slots(), fleet.epochs()),
        (0, 0),
        "nothing registered"
    );
    assert_eq!(
        idle.shift().last_jump_100ns,
        -361_000,
        "ONE hold of the drift"
    );
    assert_eq!(idle.anchor_stats().steps_followed, 0);
    assert_eq!(
        idle.frames_since_resample(),
        1,
        "the rejoin restarted the resample count"
    );
    clk.advance_ns(40_000_000);
    assert_eq!(idle.now_100ns(), busy.now_100ns());
}

#[test]
fn a_resample_waits_out_a_rejoin_hold_longer_than_a_resample_period() {
    // Alone 12 h at −94 ppm: the rejoin holds ~4.06 s, longer than the
    // 100-tick resample period. The resample waits for the hold to end (it
    // would read the rest of the hold as a backward step); the wall reads
    // monotonic throughout and registers nothing.
    let clk = VirtualClock::new(-94);
    let fleet = Arc::new(FleetShift::default());
    let mut wall = wall_on(&clk, &fleet);
    frame(&clk, &mut [&mut wall]);
    clk.advance_ns(12 * 3_600 * 1_000_000_000);
    let mut prev = wall.now_100ns();
    for i in 0..200 {
        frame(&clk, &mut [&mut wall]);
        let now = wall.now_100ns();
        assert!(now >= prev, "tick {i}: {now} < {prev}");
        prev = now;
    }
    assert_eq!((fleet.slots(), fleet.epochs()), (0, 0));
    let shift = wall.shift();
    assert_eq!(
        (shift.slots, shift.regrids, shift.last_jump_100ns),
        (0, 1, -40_608_063),
        "ONE rejoin hold of the 4.06 s drift"
    );
    assert_eq!(wall.anchor_stats().steps_followed, 0);
    assert_eq!(
        wall.frames_since_resample(),
        77,
        "the resample ran on the first tick after the hold"
    );
}

#[test]
fn a_wall_rejoins_only_after_more_than_10_s_without_a_tick() {
    let clk = VirtualClock::new(0);
    let fleet = Arc::new(FleetShift::default());
    let mut wall = wall_on(&clk, &fleet);
    frame(&clk, &mut [&mut wall]);
    clk.advance_ns(10_000_000_000);
    wall.tick();
    assert_eq!(wall.shift().regrids, 0, "exactly 10 s: a normal tick");
    clk.advance_ns(10_000_000_100);
    wall.tick();
    assert_eq!(wall.shift().regrids, 1, "10 s + 100 ns: a rejoin");
    assert_eq!(wall.shift().last_jump_100ns, 0, "no drift, nothing moved");
    assert_eq!(wall.now_100ns(), clk.truth_100ns());
}

#[test]
fn a_wall_reading_a_registered_step_1_3_ms_smaller_adopts_its_n_and_registers_nothing() {
    // Review r1 🟡 D: a wall whose line is 1.3 ms off (a lone-outlier
    // resample shortly before the step) reads the step 1.3 ms smaller than
    // the wall that registered it. Within the 2 ms step threshold that is
    // its own line's error: it adopts N = 7, its timeline moves r − 1.3 ms
    // ahead. It never registers a −1.3 ms epoch (N = −1), which would leave
    // every other wall's stamps a slot stale for good.
    let clk = VirtualClock::new(0);
    let fleet = Arc::new(FleetShift::default());
    let mut wall = wall_on(&clk, &fleet);
    frame(&clk, &mut [&mut wall]);
    // Another wall registered +260.3 ms.
    let _ = fleet.follow(0, 2_603_000);
    clk.step_utc(2_590_000);
    clk.advance_ns(FRAME_NS);
    let before = wall.now_100ns();
    wall.tick();
    assert_eq!((fleet.slots(), fleet.epochs()), (7, 1));
    assert_eq!(wall.shift().slots, 7);
    assert_eq!(wall.now_100ns() - before, 256_666, "r less the 1.3 ms");
    assert_eq!(wall.shift().last_remainder_100ns, 256_666);
}

#[test]
fn the_line_runs_on_through_a_residue_hold_the_wall_reading_freezes() {
    // The wall reads the step 1 ms smaller than the wall that registered it
    // (D(7) − 500 µs vs D(7) + 500 µs): it adopts N = 7 and holds 500 µs.
    // The reading freezes; the LINE (VBAN's clock reads it) sits 500 µs
    // below it and runs on, and they meet when the hold ends.
    let clk = VirtualClock::new(0);
    let fleet = Arc::new(FleetShift::default());
    let mut wall = wall_on(&clk, &fleet);
    frame(&clk, &mut [&mut wall]);
    assert_eq!(wall.line_100ns(), wall.now_100ns(), "no hold: one value");
    let _ = fleet.follow(0, shift_100ns(7) + 5_000);
    clk.step_utc(shift_100ns(7) - 5_000);
    clk.advance_ns(FRAME_NS);
    let before = wall.now_100ns();
    wall.tick();
    assert_eq!(wall.shift().slots, 7);
    assert_eq!(wall.shift().last_jump_100ns, -5_000, "ONE hold of 500 µs");
    assert_eq!(wall.now_100ns(), before, "the reading froze");
    assert_eq!(wall.line_100ns(), before - 5_000, "the line moved back");
    clk.advance_ns(200_000);
    assert_eq!(wall.now_100ns(), before);
    assert_eq!(wall.line_100ns(), before - 3_000, "and runs on");
    clk.advance_ns(800_000);
    assert_eq!(
        wall.line_100ns(),
        wall.now_100ns(),
        "past the hold: one value"
    );
    assert_eq!(wall.now_100ns(), before + 5_000);
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
