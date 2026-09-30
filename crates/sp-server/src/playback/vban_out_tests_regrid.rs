//! #224 part 2 (design record 5899388193): VBAN at a fleet date step, policy
//! SlewRemainder. VBAN has no timecode and its receiver (VB-Matrix at FOH)
//! paces by arrival, so a jump of its clock would still send audio at once
//! and a hold would leave a gap. The walls relabel the whole slots N, VBAN's
//! clock owes the timeline's movement (r ahead, or a residue hold, review
//! r1 🟡 G) and pays it back at `VBAN_SLEW_PPM`: every packet interval stays
//! within 4.1667 ms ± 100 ppm, in both directions, whatever the step's phase
//! against the packet grid (three consecutive blocks, review r3). Before:
//! +260 ms sent ~80 packets back to back (FINDING 5898834252), −19.8 ms left
//! a gap.
//!
//! VBAN's production clock (`WallVbanClock::slewing`) over a wall on a
//! [`VirtualClock`]; a sleep advances the virtual clock (no real wait), and
//! the sink records each packet's virtual monotonic send instant. Pins and
//! ranges derived with a scratch Python model of this rig.
//! Wired via `#[path = "vban_out_tests_regrid.rs"] mod regrid;` in
//! `vban_out_tests.rs`.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use super::*;
use crate::playback::fleet_shift::{FleetShift, STEP_RESIDUE_100NS, shift_100ns};
use crate::playback::vban_clock::VBAN_SLEW_MAX_100NS;
use crate::playback::vban_out::{
    RemainderSlew, VBAN_SLEW_PPM, VbanClock, VbanOut, VbanSender, VbanSink, WallVbanClock,
    run_vban_loop,
};
use crate::playback::wallclock::{ClockSource, VirtualClock, WallClock};
use sp_core::genlock::strict_next_boundary_100ns;

/// 4.1667 ms − 100 ppm and + 100 ppm, in ns (1 s / 240 = 4 166 666.7 ns).
const INTERVAL_MIN_NS: u128 = 4_166_250;
const INTERVAL_MAX_NS: u128 = 4_167_083;

/// VBAN's production clock over a wall on the virtual clock: its sleep
/// advances the virtual clock.
struct VirtualVban {
    clock: WallVbanClock,
    clk: Arc<VirtualClock>,
}

impl VbanClock for VirtualVban {
    fn now_100ns(&mut self) -> i64 {
        self.clock.now_100ns()
    }

    fn sleep_100ns(&mut self, d_100ns: i64) {
        self.clk
            .advance_ns(u64::try_from(d_100ns).unwrap_or(0) * 100);
    }

    fn slew_owed_100ns(&self) -> i64 {
        self.clock.slew_owed_100ns()
    }
}

/// Records each packet's virtual monotonic send instant.
struct MonoSink {
    clk: Arc<VirtualClock>,
    sent: Vec<Instant>,
}

impl VbanSink for MonoSink {
    fn send_packet(&mut self, packet: &[u8], _addr: SocketAddr) -> io::Result<usize> {
        self.sent.push(self.clk.now_monotonic());
        Ok(packet.len())
    }
}

/// What a run saw.
struct Run {
    intervals_ns: Vec<u128>,
    late_sends: u64,
    /// `slew_owed_us` of the status after each block.
    owed_us: Vec<i64>,
}

/// Send 400 program blocks (contiguous boundaries, 8 packets each) as the
/// VBAN thread does — one clock read, then the block — with the fleet date
/// step `step_100ns` landing before block `at`.
fn run(step_100ns: i64, at: usize) -> Run {
    run_after(None, step_100ns, at)
}

/// [`run`], with another wall's reading `registered` of the step (when
/// `Some`) registered right before it lands.
fn run_after(registered: Option<i64>, step_100ns: i64, at: usize) -> Run {
    let clk = VirtualClock::new(0);
    let fleet = Arc::new(FleetShift::default());
    let wall = WallClock::with_fleet(Box::new(clk.clone()), fleet.clone());
    let mut clock = VirtualVban {
        clock: WallVbanClock::slewing(wall),
        clk: clk.clone(),
    };
    let out = out_with(active_config(&["10.0.0.1:6980"]));
    let mut sink = MonoSink {
        clk: clk.clone(),
        sent: Vec::new(),
    };
    let mut sender = VbanSender::default();
    let mut due = strict_next_boundary_100ns(clock.now_100ns(), 30);
    let mut owed_us = Vec::new();
    for i in 0..400 {
        if i == at {
            if let Some(other) = registered {
                let _ = fleet.follow(0, other);
            }
            clk.step_utc(step_100ns);
        }
        clock.now_100ns();
        out.record_slew(clock.slew_owed_100ns());
        sender.send_block(&out, &block(due, 0.5), &mut sink, &mut clock);
        out.record_slew(clock.slew_owed_100ns());
        owed_us.push(out.status().slew_owed_us);
        due = strict_next_boundary_100ns(due, 30);
    }
    let intervals_ns = sink
        .sent
        .windows(2)
        .map(|w| w[1].saturating_duration_since(w[0]).as_nanos())
        .collect();
    Run {
        intervals_ns,
        late_sends: out.status().late_sends,
        owed_us,
    }
}

#[test]
fn a_date_step_either_way_keeps_every_packet_interval_within_100_ppm() {
    // (step, r in µs): +260.3 ms → 7 slots + 26.966 ms; −19.8 ms → −1 slot
    // + 13.533 ms. VBAN owes r at the follow and pays it at 40 ppm: ~399 µs
    // over the ~299 blocks (~10 s) after it. Three phases of the step
    // against the packet grid.
    for (step, r_us, at) in [(2_603_000i64, 26_966i64), (-198_000, 13_533)]
        .into_iter()
        .flat_map(|(s, r)| (100..=102).map(move |at| (s, r, at)))
    {
        let run = run(step, at);
        assert_eq!(run.intervals_ns.len(), 400 * 8 - 1);
        let bad: Vec<_> = run
            .intervals_ns
            .iter()
            .enumerate()
            .filter(|(_, ns)| !(INTERVAL_MIN_NS..=INTERVAL_MAX_NS).contains(*ns))
            .collect();
        assert!(
            bad.is_empty(),
            "{step} at {at}: intervals off 4.1667 ms ± 100 ppm: {bad:?}"
        );
        assert!(
            run.intervals_ns.iter().all(|&ns| ns >= 1_000_000),
            "{step} at {at}: no burst"
        );
        assert_eq!(run.late_sends, 0, "{step} at {at}: no late send");
        // Nothing owed before the step; r owed right after its follow
        // (its block paid at most a block's worth), then less and less.
        assert!(run.owed_us[..at].iter().all(|&o| o == 0), "{step} at {at}");
        let after = run.owed_us[at];
        assert!(
            (r_us - 5..=r_us).contains(&after),
            "{step} at {at}: owed right after the follow {after} vs r {r_us}"
        );
        assert!(
            run.owed_us[at..].windows(2).all(|w| w[1] <= w[0]),
            "{step} at {at}: paid down, never owed more"
        );
        let paid = after - run.owed_us[399];
        assert!(
            (393..=402).contains(&paid),
            "{step} at {at}: 40 ppm of ~10 s ≈ 399 µs paid, got {paid}"
        );
    }
}

#[test]
fn a_residue_hold_at_a_follow_is_slewed_too_never_a_gap() {
    // Review r1 🟡 G: VBAN's wall reads the step 1 ms smaller than the wall
    // that registered it (D(7) − 500 µs vs D(7) + 500 µs): it adopts N = 7
    // and its timeline is 500 µs short — ONE hold. VBAN's clock reads the
    // line through it and owes −500 µs, paid back at 40 ppm: no interval off
    // 100 ppm (the plain wall reading stopped one interval for the whole
    // hold: 4.667 ms). At 50 ppm a 41 668-unit interval could pay 3 × 100 ns
    // back and stretch to 4 167 100 ns (+104 ppm) at one step phase in three
    // (review r3 🟡 C).
    for at in 100..=102 {
        let run = run_after(Some(shift_100ns(7) + 5_000), shift_100ns(7) - 5_000, at);
        let bad: Vec<_> = run
            .intervals_ns
            .iter()
            .enumerate()
            .filter(|(_, ns)| !(INTERVAL_MIN_NS..=INTERVAL_MAX_NS).contains(*ns))
            .collect();
        assert!(bad.is_empty(), "at {at}: intervals off ± 100 ppm: {bad:?}");
        assert_eq!(run.late_sends, 0, "at {at}");
        assert!(run.owed_us[..at].iter().all(|&o| o == 0), "at {at}");
        let after = run.owed_us[at];
        assert!(
            (-500..=-495).contains(&after),
            "at {at}: owed right after the follow {after}: the hold, negative"
        );
        assert!(
            run.owed_us[at..].windows(2).all(|w| w[1] >= w[0]),
            "at {at}: paid toward 0, never owed more"
        );
        assert!(
            (-110..=-90).contains(&run.owed_us[399]),
            "at {at}: ~10 s at 40 ppm pays ~400 of the 500 µs"
        );
    }
}

#[test]
fn the_remainder_slew_pays_a_hold_back_toward_zero_never_past_it() {
    let t = 17_900_000_000_000_000i64;
    let mut slew = RemainderSlew::default();
    // The line moved BACK 5 000 (a hold), read `t` after the movement.
    slew.owe(-5_000, t);
    assert_eq!(slew.owed_at(t), -5_000);
    assert_eq!(
        slew.clock_100ns(t),
        t + 5_000,
        "VBAN's clock reads the instant before the movement"
    );
    assert_eq!(slew.owed_at(t + 24_999), -5_000);
    assert_eq!(slew.owed_at(t + 25_000), -4_999);
    assert_eq!(slew.owed_at(t - 5), -5_000, "never more than owed");
    let all_paid = 5_000 * 25_000;
    assert_eq!(slew.owed_at(t + all_paid - 25_000), -1);
    assert_eq!(slew.owed_at(t + all_paid), 0);
    assert_eq!(slew.owed_at(t + 2 * all_paid), 0, "never past 0");
    // A jump ahead while a hold is still owed: they net out.
    let t2 = t + 200_000;
    let v = slew.clock_100ns(t2);
    slew.owe(269_666, t2 + 269_666);
    assert_eq!(slew.owed_at(t2 + 269_666), -4_992 + 269_666);
    assert_eq!(slew.clock_100ns(t2 + 269_666), v);
}

#[test]
fn a_rejoin_and_a_follow_in_one_tick_are_both_owed() {
    // Review r2: VBAN's thread stalled 11 s across a +260.3 ms step the busy
    // wall has not followed yet. VBAN's first tick rejoins the busy wall's
    // pre-step line (at +94 ppm a 9 399 jump, at −94 ppm a 9 400 HOLD that
    // the follow then re-anchors through, review r3 🟡 A), and its probe
    // follows the step: VBAN owes the LINE's whole movement in that tick,
    // so its clock runs on by exactly the elapsed time. A second step
    // (+50 ms) is owed on top, less 13 paid over the frame at 40 ppm (pins
    // from a scratch Python model).
    for (ppm, owed, owed_again) in [(94, 280_193, 446_879), (-94, 259_138, 425_760)] {
        rejoin_and_follow(ppm, owed, owed_again);
    }
}

fn rejoin_and_follow(ppm: i64, owed: i64, owed_again: i64) {
    let clk = VirtualClock::new(ppm);
    let fleet = Arc::new(FleetShift::default());
    let mut busy = WallClock::with_fleet(Box::new(clk.clone()), fleet.clone());
    let wall = WallClock::with_fleet(Box::new(clk.clone()), fleet.clone());
    let mut vban = VirtualVban {
        clock: WallVbanClock::slewing(wall),
        clk: clk.clone(),
    };
    for _ in 0..5 {
        clk.advance_ns(33_333_300);
        busy.tick();
        vban.now_100ns();
    }
    let before = vban.now_100ns();
    let t0 = clk.now_monotonic();
    for _ in 0..330 {
        clk.advance_ns(33_333_300);
        busy.tick();
    }
    clk.step_utc(2_603_000);
    clk.advance_ns(33_333_300);
    let after = vban.now_100ns();
    let elapsed = |from: Instant| {
        i64::try_from(clk.now_monotonic().duration_since(from).as_nanos() / 100).unwrap()
    };
    assert_eq!(fleet.slots(), 7, "{ppm}");
    assert_eq!(vban.slew_owed_100ns(), owed, "{ppm}: the line's movement");
    assert_eq!(
        after - before,
        elapsed(t0),
        "{ppm}: neither a jump nor a stop"
    );
    let t1 = clk.now_monotonic();
    clk.step_utc(500_000);
    clk.advance_ns(33_333_300);
    let again = vban.now_100ns();
    assert_eq!(fleet.slots(), 8, "{ppm}");
    assert_eq!(
        vban.slew_owed_100ns(),
        owed_again,
        "{ppm}: + r, less 13 paid"
    );
    assert_eq!(again - after, elapsed(t1) + 13, "{ppm}: the 13 paid");
}

#[test]
fn a_step_the_resample_reads_first_is_owed_whole() {
    // Review r3 🟡 A: the step lands on the wall's 100th tick. The resample
    // applies (and arms) the bounded 1 ms, the probe follows the rest in the
    // same tick. VBAN owes the line's whole movement r = 269 666, so its
    // clock neither jumps 1 ms (a 3.17 ms packet interval) nor stops.
    let clk = VirtualClock::new(0);
    let wall = WallClock::with_fleet(Box::new(clk.clone()), Arc::new(FleetShift::default()));
    let mut vban = VirtualVban {
        clock: WallVbanClock::slewing(wall),
        clk: clk.clone(),
    };
    vban.now_100ns();
    for _ in 0..100 {
        clk.advance_ns(33_333_300);
        vban.now_100ns();
    }
    let before = vban.now_100ns();
    let t0 = clk.now_monotonic();
    clk.step_utc(2_603_000);
    clk.advance_ns(33_333_300);
    let after = vban.now_100ns();
    let elapsed = clk.now_monotonic().duration_since(t0).as_nanos() / 100;
    assert_eq!(vban.slew_owed_100ns(), 269_666, "r, not r − 1 ms");
    assert_eq!(after - before, i64::try_from(elapsed).unwrap());
}

#[test]
fn a_movement_over_the_cap_is_taken_at_once_and_counted() {
    // Review r3 🟡 B: VBAN's thread stalled 12 h, alone, at −94 ppm: its
    // rejoin moves the line by −4.06 s. That is taken at once (counted,
    // one WARN), never owed.
    let clk = VirtualClock::new(-94);
    let wall = WallClock::with_fleet(Box::new(clk.clone()), Arc::new(FleetShift::default()));
    let mut vban = VirtualVban {
        clock: WallVbanClock::slewing(wall),
        clk: clk.clone(),
    };
    vban.now_100ns();
    clk.advance_ns(33_333_300);
    vban.now_100ns();
    assert_eq!(vban.clock.taken_at_once(), 0);
    clk.advance_ns(12 * 3_600 * 1_000_000_000);
    vban.now_100ns();
    assert_eq!(vban.clock.taken_at_once(), 1);
    assert_eq!(vban.slew_owed_100ns(), 0);
}

#[test]
fn a_movement_taken_at_once_leaves_the_debt_paying_where_it_was() {
    // Review r3: the line jumps with the movement taken at once, so the
    // debt's elapsed line moves with it — never paid or re-owed at once.
    let t = 17_900_000_000_000_000i64;
    let mut slew = RemainderSlew::default();
    assert!(slew.owe(269_666, t));
    let t2 = t + 200_000;
    let owed = slew.owed_at(t2);
    assert_eq!(owed, 269_658, "40 ppm: 8 paid over 20 ms");
    assert!(!slew.owe(-40_000_000, t2 - 40_000_000), "a 4 s jump back");
    assert_eq!(slew.owed_at(t2 - 40_000_000), owed);
    assert_eq!(slew.owed_at(t2 - 40_000_000 + 25_000), owed - 1);
}

#[test]
fn a_movement_over_one_slot_is_taken_at_once_never_slewed_for_hours() {
    assert_eq!(VBAN_SLEW_MAX_100NS, shift_100ns(1) + STEP_RESIDUE_100NS);
    let t = 17_900_000_000_000_000i64;
    let mut slew = RemainderSlew::default();
    assert!(slew.owe(VBAN_SLEW_MAX_100NS, t), "one slot + 3 ms: owed");
    assert_eq!(slew.owed_at(t), 363_334);
    let mut slew = RemainderSlew::default();
    assert!(
        !slew.owe(-VBAN_SLEW_MAX_100NS - 1, t),
        "more: taken at once"
    );
    assert_eq!(slew.owed_at(t), 0);
    assert_eq!(slew.clock_100ns(t), t);
}

#[test]
fn the_remainder_slew_owes_a_jump_and_pays_it_back_at_40_ppm() {
    let t = 17_900_000_000_000_000i64;
    let mut slew = RemainderSlew::default();
    assert_eq!(slew.owed_at(t), 0);
    assert_eq!(slew.clock_100ns(t), t, "nothing owed: the timeline itself");
    // The timeline jumped r = 269 666 forward, read `t` after the jump.
    slew.owe(269_666, t);
    assert_eq!(slew.owed_at(t), 269_666);
    assert_eq!(
        slew.clock_100ns(t),
        t - 269_666,
        "VBAN's clock reads the instant before the jump"
    );
    // 40 ppm: 100 ns paid per 2.5 ms (25 000 × 100 ns) of timeline.
    assert_eq!(slew.owed_at(t + 24_999), 269_666);
    assert_eq!(slew.owed_at(t + 25_000), 269_665);
    assert_eq!(slew.owed_at(t + 200_000), 269_658);
    assert_eq!(slew.owed_at(t - 5), 269_666, "never more than owed");
    let all_paid = 269_666 * 25_000;
    assert_eq!(slew.owed_at(t + all_paid - 25_000), 1);
    assert_eq!(slew.owed_at(t + all_paid), 0);
    assert_eq!(slew.owed_at(t + 2 * all_paid), 0, "never negative");
    // A second jump while still owing: owed on top of the rest, and the
    // clock still does not jump.
    let t2 = t + 200_000;
    let v = slew.clock_100ns(t2);
    slew.owe(135_333, t2 + 135_333);
    assert_eq!(slew.owed_at(t2 + 135_333), 269_658 + 135_333);
    assert_eq!(slew.clock_100ns(t2 + 135_333), v);
    assert_eq!(VBAN_SLEW_PPM, 40);
}

#[test]
fn the_ndi_input_clock_follows_the_wall_and_owes_nothing() {
    // `WallVbanClock::new` (the NDI input's) keeps following its wall: at a
    // step its reading jumps by r — the input's next boundary comes at most
    // one early — and it never slews.
    let clk = VirtualClock::new(0);
    let wall = WallClock::with_fleet(Box::new(clk.clone()), Arc::new(FleetShift::default()));
    let mut plain = VirtualVban {
        clock: WallVbanClock::new(wall),
        clk: clk.clone(),
    };
    plain.now_100ns();
    clk.advance_ns(33_333_300);
    let before = plain.now_100ns();
    clk.step_utc(2_603_000);
    clk.advance_ns(33_333_300);
    let after = plain.now_100ns();
    assert_eq!(after - before, 333_333 + 269_666, "one slot + r");
    assert_eq!(plain.slew_owed_100ns(), 0);
    // VBAN's own (slewing) clock over the same kind of wall does not jump.
    let clk = VirtualClock::new(0);
    let wall = WallClock::with_fleet(Box::new(clk.clone()), Arc::new(FleetShift::default()));
    let mut vban = VirtualVban {
        clock: WallVbanClock::slewing(wall),
        clk: clk.clone(),
    };
    vban.now_100ns();
    clk.advance_ns(33_333_300);
    let before = vban.now_100ns();
    clk.step_utc(2_603_000);
    clk.advance_ns(33_333_300);
    let after = vban.now_100ns();
    assert_eq!(after - before, 333_333, "one slot: the jump is owed");
    assert_eq!(vban.slew_owed_100ns(), 269_666);
}

#[test]
fn the_vban_loop_publishes_what_its_clock_still_owes() {
    let out = VbanOut::new();
    out.stop();
    let mut clock = FakeClock::at(D);
    clock.owed = 123_456;
    let mut sink = RecordingSink::on(&clock);
    run_vban_loop(&out, &mut sink, &mut clock);
    assert_eq!(out.status().slew_owed_us, 12_345);
}
