//! #224 part 2 (design record 5899388193): VBAN at a fleet date step, policy
//! SlewRemainder. VBAN has no timecode and its receiver (VB-Matrix at FOH)
//! paces by arrival, so a jump of its clock would still send audio at once.
//! The walls relabel the whole slots N, VBAN's clock owes the remainder r and
//! pays it back at `VBAN_SLEW_PPM`: every packet interval stays within
//! 4.1667 ms ± 100 ppm, in both directions. Before: +260 ms sent ~80 packets
//! back to back (FINDING 5898834252), −19.8 ms left a gap.
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
use crate::playback::fleet_shift::FleetShift;
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
    owed_us: Vec<u64>,
}

/// Send 400 program blocks (contiguous boundaries, 8 packets each) as the
/// VBAN thread does — one clock read, then the block — with the fleet date
/// step `step_100ns` landing before block 100.
fn run(step_100ns: i64) -> Run {
    let clk = VirtualClock::new(0);
    let wall = WallClock::with_fleet(Box::new(clk.clone()), Arc::new(FleetShift::default()));
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
        if i == 100 {
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
    // + 13.533 ms. VBAN owes r at the follow and pays it at 50 ppm: ~499 µs
    // over the 299 blocks (~10 s) after it.
    for (step, r_us) in [(2_603_000i64, 26_966u64), (-198_000, 13_533)] {
        let run = run(step);
        assert_eq!(run.intervals_ns.len(), 400 * 8 - 1);
        let bad: Vec<_> = run
            .intervals_ns
            .iter()
            .enumerate()
            .filter(|(_, ns)| !(INTERVAL_MIN_NS..=INTERVAL_MAX_NS).contains(*ns))
            .collect();
        assert!(
            bad.is_empty(),
            "{step}: intervals off 4.1667 ms ± 100 ppm: {bad:?}"
        );
        assert!(
            run.intervals_ns.iter().all(|&ns| ns >= 1_000_000),
            "{step}: no burst"
        );
        assert_eq!(run.late_sends, 0, "{step}: no late send");
        // Nothing owed before the step; r owed right after its follow
        // (block 100 paid at most a block's worth), then less and less.
        assert!(run.owed_us[..99].iter().all(|&o| o == 0), "{step}");
        let after = run.owed_us[100];
        assert!(
            (r_us - 5..=r_us).contains(&after),
            "{step}: owed right after the follow {after} vs r {r_us}"
        );
        assert!(
            run.owed_us[100..].windows(2).all(|w| w[1] <= w[0]),
            "{step}: paid down, never owed more"
        );
        let paid = after - run.owed_us[399];
        assert!(
            (490..=505).contains(&paid),
            "{step}: 50 ppm of ~10 s ≈ 499 µs paid, got {paid}"
        );
    }
}

#[test]
fn the_remainder_slew_owes_a_jump_and_pays_it_back_at_50_ppm() {
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
    // 50 ppm: 100 ns paid per 2 ms (20 000 × 100 ns) of timeline.
    assert_eq!(slew.owed_at(t + 19_999), 269_666);
    assert_eq!(slew.owed_at(t + 20_000), 269_665);
    assert_eq!(slew.owed_at(t + 200_000), 269_656);
    assert_eq!(slew.owed_at(t - 5), 269_666, "never more than owed");
    let all_paid = 269_666 * 20_000;
    assert_eq!(slew.owed_at(t + all_paid - 20_000), 1);
    assert_eq!(slew.owed_at(t + all_paid), 0);
    assert_eq!(slew.owed_at(t + 2 * all_paid), 0, "never negative");
    // A second jump while still owing: owed on top of the rest, and the
    // clock still does not jump.
    let t2 = t + 200_000;
    let v = slew.clock_100ns(t2);
    slew.owe(135_333, t2 + 135_333);
    assert_eq!(slew.owed_at(t2 + 135_333), 269_656 + 135_333);
    assert_eq!(slew.clock_100ns(t2 + 135_333), v);
    assert_eq!(VBAN_SLEW_PPM, 50);
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
