//! #224 part 2 (design record 5899388193): a fleet date step whose walls
//! follow at DIFFERENT ticks costs the paced output and the program bus
//! nothing: no program fill, no late drop, no program coalesce, no resync, no
//! handoff coalesce.
//!
//! One virtual clock, one relabel registry, three walls: the source pacer's,
//! its paced submit consumer's and the `SP-program` sender's. The step lands
//! right after a boundary; the program wall follows it at its next check (1 ms
//! after the boundary) and the pacer's wall one boundary LATER (its probe that
//! tick is preempted: a wide read, rejected). That is the order that used to
//! hurt: the program jumped the whole +260 ms first and filled the source's
//! boundaries (the 3-slot grace ran out), then the source caught its 7 slots
//! up back to back — the late drops, and a burst the 2-deep handoff coalesced.
//! Now each wall moves only by r (< one slot, well inside the grace).
//!
//! The consumer runs when the pacer sleeps (a burst of emits at one instant
//! queues up, as it would against a consumer thread mid-submit). Pins derived
//! with a scratch Python model of this rig.

use std::cell::Cell;
use std::sync::Arc;

use super::*;
use crate::playback::fleet_shift::FleetShift;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::paced_output::{
    BoundaryOut, ConsumerStep, HandoffSink, PacedConsumer, PacedFeed, Picture, SharedHandoff,
};
use crate::playback::pacer::{PacedFrame, PacedSink, Pacer, ServiceOutcome, plan_sleep_100ns};
use crate::playback::program_output::BoundaryTicker;
use crate::playback::program_transition::ActiveWindow;
use crate::playback::submit_handoff::{SUBMIT_HANDOFF_BOUND, SubmitJob};
use crate::playback::wallclock::{VirtualClock, WallClock};
use sp_core::genlock::{GENLOCK_GRID_FPS, floor_boundary_100ns, strict_next_boundary_100ns};
use sp_ndi::AudioFrame;

/// The source's program id (unused anywhere else, so the process-wide bus the
/// consumer also offers to never owns it).
const SRC: i64 = 22_411;
/// The program sender checks this long after a boundary (1 ms).
const CHECK_100NS: i64 = 10_000;

/// Frame `j` of a 30-fps source: pts on the `j`-th grid boundary, 4×2 NV12,
/// one stereo boundary of audio.
fn frame(j: i64) -> PacedFrame {
    PacedFrame {
        pts_ns: j * 10_000_000 / 30 * 100,
        width: 4,
        height: 2,
        stride: 4,
        video: SharedFrame::new(vec![0u8; 12]),
        audio: vec![AudioFrame {
            data: vec![0.25; 3200],
            channels: 2,
            sample_rate: 48_000,
            timecode_100ns: None,
        }],
    }
}

/// The pacer's sink: the paced output's handoff, and the program core the
/// consumer's delivery would offer the same pair to (the consumer itself
/// delivers to [`Discard`]: the core is fed here, at emit time).
struct Tee<'a> {
    out: HandoffSink<'a>,
    core: &'a mut ProgramCore,
}

impl PacedSink for Tee<'_> {
    fn emit(&mut self, video: &PacedFrame, audio: &[AudioFrame], v: i64, a: i64) {
        let copy = SubmitJob::from_paced(video, audio, v, a, true);
        self.core.offer(SRC, copy);
        self.out.emit(video, audio, v, a);
    }
}

/// A paced output that delivers nowhere: the [`Tee`] feeds the core.
struct Discard;

impl BoundaryOut for Discard {
    fn deliver(&mut self, _playlist_id: i64, _job: SubmitJob) {}
}

/// What the rig counted.
struct Outcome {
    health: ProgramHealth,
    handoff_dropped: u64,
    unserviced: u64,
    pacer_resyncs: u64,
    program_stamps: Vec<i64>,
    /// K of the pacer's wall and of the program's.
    slots: [i64; 2],
}

/// 300 boundaries with the step `step_100ns` right after the 150th.
fn run(step_100ns: i64) -> Outcome {
    let clk = VirtualClock::new(0);
    let fleet = Arc::new(FleetShift::default());
    let wall = || WallClock::with_fleet(Box::new(clk.clone()), fleet.clone());
    let black = Picture {
        width: 4,
        height: 2,
        stride: 4,
        video: SharedFrame::new(vec![16u8; 12]),
    };
    let mut consumer = PacedConsumer::new(Discard, SRC, wall(), black);
    let mut pacer = Pacer::with_wallclock(30, true, wall());
    let mut program_wall = wall();
    let mut ticker = BoundaryTicker::default();
    let mut core = ProgramCore::new();
    core.select_initial(SRC);
    let handoff = SharedHandoff::new(SUBMIT_HANDOFF_BOUND);
    let feed = PacedFeed::attach(&handoff);
    pacer.anchor();
    let next = Cell::new(0i64);
    let mut program_stamps = Vec::new();
    let mut services = 0u64;
    for _ in 0..3_000 {
        if services == 300 {
            break;
        }
        let interval = pacer.interval_100ns();
        let plan = plan_sleep_100ns(pacer.now_100ns(), pacer.next_boundary_100ns(), interval);
        // A relatch (a backward wall step, never here) sleeps nothing.
        let sleep = if plan.relatch { 0 } else { plan.sleep_100ns };
        if sleep > 0 && services > 0 {
            // The consumer submits what is queued; then the program checks.
            loop {
                let step = handoff.step_now(consumer.now_100ns());
                if matches!(step, ConsumerStep::Wait(_) | ConsumerStep::Exit) {
                    break;
                }
                consumer.serve(&handoff, step);
            }
            let first = sleep.min(CHECK_100NS);
            clk.advance_ns(first as u64 * 100);
            let now = program_wall.now_100ns();
            for _ in 0..ticker.advance(now) {
                program_wall.tick();
            }
            core.release(program_wall.now_100ns());
            while let Some(job) = core.take() {
                program_stamps.push(job.stamp_100ns());
            }
            clk.advance_ns((sleep - first) as u64 * 100);
        } else {
            clk.advance_ns(sleep as u64 * 100);
        }
        let mut tee = Tee {
            out: feed.sink(),
            core: &mut core,
        };
        let outcome = pacer.service(
            || {
                let j = next.get();
                next.set(j + 1);
                Some(frame(j))
            },
            &mut tee,
        );
        match outcome {
            ServiceOutcome::Wait { until_100ns }
            | ServiceOutcome::Reanchored { until_100ns, .. } => {
                let plan = plan_sleep_100ns(pacer.now_100ns(), until_100ns, interval);
                if !plan.relatch {
                    clk.advance_ns(plan.sleep_100ns as u64 * 100);
                }
            }
            ServiceOutcome::Emitted | ServiceOutcome::Repeated | ServiceOutcome::Starved => {
                services += 1;
                if services == 150 {
                    clk.step_utc(step_100ns);
                    // The pacer's probe this tick is preempted: it follows
                    // one boundary after the program wall.
                    clk.delay_next_reads(&[400_000]);
                }
                pacer.tick_wall();
            }
        }
    }
    assert_eq!(services, 300, "the rig serviced every boundary");
    let counters = handoff.snapshot().0;
    Outcome {
        health: core.status().health,
        handoff_dropped: counters.dropped,
        unserviced: counters.song_change_unserviced_slots,
        pacer_resyncs: pacer.stats().resyncs,
        program_stamps,
        slots: [pacer.stats().fleet_shift_slots, program_wall.shift().slots],
    }
}

#[test]
fn walls_following_a_date_step_at_different_ticks_cost_no_fill_drop_or_coalesce() {
    // +260.3 ms (the 20:58Z step), −19.8 ms (a date-master restart), the
    // nightly −1.5 s and +90 ms.
    for (step, slots) in [
        (2_603_000, 7),
        (-198_000, -1),
        (-15_000_000, -45),
        (900_000, 2),
    ] {
        let o = run(step);
        let h = o.health;
        assert_eq!(
            (h.filled, h.late_dropped, h.coalesced, h.resyncs),
            (0, 0, 0, 0),
            "{step}: the program filled, dropped or coalesced nothing"
        );
        assert_eq!(h.forwarded, 300, "{step}: every source boundary forwarded");
        assert_eq!(o.handoff_dropped, 0, "{step}: no catch-up burst coalesced");
        assert_eq!(o.unserviced, 0, "{step}");
        assert_eq!(o.pacer_resyncs, 0, "{step}");
        assert!(
            o.program_stamps
                .windows(2)
                .all(|w| w[1] == strict_next_boundary_100ns(w[0], GENLOCK_GRID_FPS)),
            "{step}: the program's stamps are contiguous"
        );
        assert_eq!(o.slots[0], slots, "{step}: the pacer wall relabelled");
        assert_eq!(o.slots[1], slots, "{step}: the program wall relabelled");
    }
}

#[test]
fn the_status_shows_every_stamp_it_carries_on_the_wire() {
    // The bus decides on internal stamps; `GET /api/v1/program` (and every
    // log that prints the status) shows the stamp a receiver sees.
    let fleet = FleetShift::default();
    assert_eq!(fleet.follow(0, 2_603_000).slots, 7);
    let b = |k: i64| {
        let mut x = floor_boundary_100ns(17_900_000_000_000_000, GENLOCK_GRID_FPS);
        for _ in 0..k {
            x = strict_next_boundary_100ns(x, GENLOCK_GRID_FPS);
        }
        x
    };
    let mut core = ProgramCore::new();
    core.select_initial(SRC);
    let idle = core.status().on_wire(&fleet);
    assert_eq!(idle.health.last_stamp_100ns, 0, "none yet stays none");
    assert_eq!(idle.cut_boundary_100ns, None);
    assert_eq!(idle.transition.active, None);
    let mut st = core.status();
    st.cut_boundary_100ns = Some(b(3));
    st.health.last_stamp_100ns = b(2);
    st.transition.active = Some(ActiveWindow {
        from: Some(SRC),
        to: 5,
        start_boundary_100ns: b(4),
        n_slots: 9,
        served_slots: 1,
        progress: 11,
    });
    let wire = st.on_wire(&fleet);
    assert_eq!(wire.cut_boundary_100ns, Some(b(10)), "7 slots relabelled");
    assert_eq!(wire.health.last_stamp_100ns, b(9));
    assert_eq!(
        wire.transition.active.map(|w| w.start_boundary_100ns),
        Some(b(11))
    );
}
