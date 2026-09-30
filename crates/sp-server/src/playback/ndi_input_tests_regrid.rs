//! #224 part 2 (design record 5899388193): the NDI input "OBS manuál" at a
//! fleet date step. Its grid wall (the program wall domain, a
//! `WallVbanClock` that follows its wall) relabels the whole slots and moves
//! its timeline by the remainder r only, so its next boundary comes at most
//! ONE boundary early (r < one slot) — never a catch-up of the step's slots
//! back to back. The loop runs `run_input_loop` on its own thread over a
//! virtual clock: a sleep advances it, and each clock read takes the jobs the
//! bus got since the last one, with the virtual instant they were serviced
//! at. A child of `ndi_input_tests.rs`, sharing its rig.

use std::sync::Arc;
use std::thread;
use std::time::Instant;

use super::*;
use crate::playback::fleet_shift::FleetShift;
use crate::playback::ndi_input::{NdiInputShared, run_input_loop};
use crate::playback::program_bus::ProgramBus;
use crate::playback::vban_out::{VbanClock, WallVbanClock};
use crate::playback::wallclock::{ClockSource, VirtualClock, WallClock};

/// The input's grid clock over a virtual wall (see the module doc).
struct SteppedClock {
    clock: WallVbanClock,
    clk: Arc<VirtualClock>,
    bus: Arc<ProgramBus>,
    shared: Arc<NdiInputShared>,
    /// `(the true UTC it lands at, the step)`, once.
    step: Option<(i64, i64)>,
    /// Stop the loop once this many boundaries were serviced.
    stop_after: usize,
    /// `(virtual instant, stamp)` of every serviced boundary.
    services: Vec<(Instant, i64)>,
}

impl VbanClock for SteppedClock {
    fn now_100ns(&mut self) -> i64 {
        let at = self.clk.now_monotonic();
        for job in drain(&self.bus) {
            self.services.push((at, job.video_tc_100ns));
        }
        if self.services.len() >= self.stop_after {
            self.shared.stop();
        }
        self.clock.now_100ns()
    }

    fn sleep_100ns(&mut self, d_100ns: i64) {
        self.clk
            .advance_ns(u64::try_from(d_100ns).unwrap_or(0) * 100);
        if let Some((at, step)) = self.step
            && self.clk.truth_100ns() >= at
        {
            self.clk.step_utc(step);
            self.step = None;
        }
    }

    fn slew_owed_100ns(&self) -> i64 {
        self.clock.slew_owed_100ns()
    }
}

/// Run the loop over a +/− `step_100ns` landing on `b(20)`; the serviced
/// boundaries and the input's status.
fn run(step_100ns: i64) -> (Vec<(Instant, i64)>, NdiInputStatus) {
    let rig = rig(source_frames(30, 30), (0..30).map(Some).collect());
    let Rig {
        shared, bus, input, ..
    } = rig;
    let clk = VirtualClock::new(0);
    // Start the true UTC just after the rig's `b(0)` (the boundary it
    // connected at), before the wall anchors: no step for the wall.
    clk.step_utc(b(0) + 1_000 - clk.truth_100ns());
    let wall = WallClock::with_fleet(Box::new(clk.clone()), Arc::new(FleetShift::default()));
    let clock = SteppedClock {
        clock: WallVbanClock::new(wall),
        clk,
        bus: bus.clone(),
        shared: shared.clone(),
        step: Some((b(20), step_100ns)),
        stop_after: 40,
        services: Vec::new(),
    };
    let loop_bus = bus.clone();
    let handle = thread::spawn(move || {
        let (mut input, mut clock) = (input, clock);
        run_input_loop(&mut input, &loop_bus, &mut clock);
        clock
    });
    let clock = handle.join().expect("the loop stops on the flag");
    (clock.services, shared.status(&enabled()))
}

#[test]
fn a_date_step_brings_the_inputs_next_boundary_at_most_one_early_never_a_burst() {
    // (step, r): +260.3 ms → 7 slots + 26.97 ms; −19.8 ms → −1 slot +
    // 13.53 ms. The boundary after the step's comes r early; every other one
    // a slot after the one before.
    for (step, r) in [(2_603_000i64, 269_666u128), (-198_000, 135_333)] {
        let (services, status) = run(step);
        assert!(services.len() >= 40, "{step}");
        let stamps: Vec<i64> = services.iter().map(|s| s.1).collect();
        assert_eq!(
            stamps[..40],
            (1..=40).map(b).collect::<Vec<_>>(),
            "{step}: every boundary, contiguous"
        );
        assert_eq!((status.resyncs, status.relatches), (0, 0), "{step}");
        let gaps: Vec<u128> = services[..40]
            .windows(2)
            .map(|w| w[1].0.saturating_duration_since(w[0].0).as_nanos() / 100)
            .collect();
        assert!(
            gaps.iter().all(|&g| g > 0),
            "{step}: never two boundaries at one instant: {gaps:?}"
        );
        let early: Vec<(usize, u128)> = gaps
            .iter()
            .enumerate()
            .filter(|(_, g)| !(333_333..=333_334).contains(*g))
            .map(|(i, g)| (i, *g))
            .collect();
        assert_eq!(early.len(), 1, "{step}: one short interval only: {early:?}");
        assert!(
            (333_333 - r..=333_334 - r).contains(&early[0].1),
            "{step}: that interval is one slot less r: {early:?}"
        );
    }
}
