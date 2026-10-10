//! #221 lane 3: a playlist pipeline's paced output delivers every boundary
//! to the program bus — its ONLY output (a playlist has no NDI sender of its
//! own; `SP-program` takes it off the bus). Driven single-threaded like
//! `paced_output_tests.rs`, over a real `ProgramBus` of the test's own (the
//! process-wide one is a `OnceLock`, `InstalledBus`), through
//! [`offer_to_bus`], the decision `InstalledBus` delivers with.

use super::*;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::pacer::{PacedFrame, Pacer, ServiceOutcome};
use crate::playback::program_bus::{OfferOutcome, ProgramBus, ProgramJob, Take};
use crate::playback::submit_handoff::{SUBMIT_HANDOFF_BOUND, SubmitJob, merge_pacing_stats};
use crate::playback::wallclock::{SettableClock, WallClock};
use sp_ndi::AudioFrame;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

const ON_PROGRAM: i64 = 7;
const OFF_PROGRAM: i64 = 9;

/// The k-th exact-rational grid boundary at 30 fps, in 100-ns units.
fn b(k: i64) -> i64 {
    k * 10_000_000 / 30
}

/// The tests' [`BoundaryOut`]: a bus of the test's own, through the same
/// decision production uses.
#[derive(Clone)]
struct BusOut(Arc<ProgramBus>);

impl BoundaryOut for BusOut {
    fn deliver(&mut self, playlist_id: i64, job: SubmitJob) {
        offer_to_bus(&self.0, playlist_id, job);
    }
}

/// A `w`×2 song frame at `pts_ns` with one boundary of stereo audio.
fn frame(w: u32, pts_ns: i64) -> PacedFrame {
    PacedFrame {
        pts_ns,
        width: w,
        height: 2,
        stride: w,
        video: SharedFrame::new(vec![0u8; (w * 2 * 3 / 2) as usize]),
        audio: vec![AudioFrame {
            data: vec![0.25; 1600 * 2],
            channels: 2,
            sample_rate: 48_000,
            timecode_100ns: None,
        }],
    }
}

/// A submit job stamped `stamp` with an 8×2 picture and one stereo block.
fn job(stamp: i64) -> SubmitJob {
    SubmitJob {
        width: 8,
        height: 2,
        stride: 8,
        video: SharedFrame::new(vec![0u8; 24]),
        audio: vec![AudioFrame {
            data: vec![0.5; 3200],
            channels: 2,
            sample_rate: 48_000,
            timecode_100ns: None,
        }],
        video_tc_100ns: stamp,
        audio_tc_100ns: stamp,
        live: true,
        media_pts_100ns: None,
    }
}

/// The 4×2 standby black a fill would show.
fn black() -> Picture {
    Picture {
        width: 4,
        height: 2,
        stride: 4,
        video: SharedFrame::new(vec![16u8; 12]),
    }
}

/// Every boundary the bus queued for `SP-program`: `(stamp, picture width)`
/// (0 for the program's own standby).
fn queued(bus: &ProgramBus) -> Vec<(i64, u32)> {
    let mut out = Vec::new();
    while let Take::Job(job, _) = bus.take_timeout(Duration::ZERO) {
        match job {
            ProgramJob::Source(j) => out.push((j.video_tc_100ns, j.width)),
            ProgramJob::Standby { stamp_100ns } => out.push((stamp_100ns, 0)),
            ProgramJob::Mix(_) => panic!("a selected source never mixes"),
        }
    }
    out
}

/// Run `consumer` until it waits at wall time `now`.
fn drain<O: BoundaryOut>(handoff: &SharedHandoff, consumer: &mut PacedConsumer<O>, now: i64) {
    for _ in 0..64 {
        let step = handoff.step_now(now);
        if matches!(step, ConsumerStep::Wait(_) | ConsumerStep::Exit) {
            return;
        }
        consumer.serve(handoff, step);
    }
    panic!("the consumer never waited");
}

/// A playlist on program: its decoded song goes pacer → handoff → consumer →
/// the program bus, every boundary on its stamp, and the pacing telemetry
/// still advances on both sides (the pacer's grid, the consumer's
/// deliveries) — what `/api/v1/ndi/health` reports as `pacing`.
#[test]
fn a_playlist_on_program_reaches_sp_program_through_its_paced_output() {
    let bus = Arc::new(ProgramBus::new());
    bus.select_initial(ON_PROGRAM, None);
    let (pacer_wall, clk) = WallClock::settable(0);
    let consumer_wall = WallClock::new(Box::new(clk.clone()));
    let handoff = SharedHandoff::new(SUBMIT_HANDOFF_BOUND);
    let mut consumer = PacedConsumer::new(BusOut(bus.clone()), ON_PROGRAM, consumer_wall, black());
    let mut pacer = Pacer::with_wallclock(30, true, pacer_wall);
    clk.set(0);
    pacer.anchor(); // the first boundary is b(1)

    let mut song: VecDeque<PacedFrame> =
        (0..3).map(|j| frame(8, (b(1 + j) - b(1)) * 100)).collect();
    {
        let feed = PacedFeed::attach(&handoff);
        for k in 1..=3 {
            clk.set(b(k));
            let mut sink = feed.sink();
            let out = pacer.service(|| song.pop_front(), &mut sink);
            assert_eq!(out, ServiceOutcome::Emitted, "k={k}");
            drain(&handoff, &mut consumer, b(k));
        }
    }

    assert_eq!(
        queued(&bus),
        vec![(b(1), 8), (b(2), 8), (b(3), 8)],
        "every boundary of the song reached the program bus, on its stamp"
    );
    let (counters, last_delivery) = handoff.snapshot();
    assert_eq!(counters.submitted, 3, "the consumer counts each delivery");
    assert!(
        last_delivery.is_some(),
        "the heartbeat's staleness baseline"
    );
    let stats = merge_pacing_stats(pacer.stats(), &counters);
    assert_eq!(stats.seq, 3, "the pacer serviced every boundary");
    assert_eq!(stats.late_frames, 0, "each delivered on its boundary");
    assert_eq!(stats.dropped, 0);
}

/// A playlist OFF program still has its boundaries serviced (its grid, its
/// preview, its counters), but the bus queues none of them: it only records
/// the source's progress, so a later cut finds it live. The bus takes the
/// on-program playlist's pair as before.
#[test]
fn a_playlist_off_program_feeds_the_bus_only_its_progress() {
    let bus = Arc::new(ProgramBus::new());
    bus.select_initial(ON_PROGRAM, None);
    let (wall, clk) = WallClock::settable(0);
    let handoff = SharedHandoff::new(SUBMIT_HANDOFF_BOUND);
    let mut consumer = PacedConsumer::new(BusOut(bus.clone()), OFF_PROGRAM, wall, black());
    let feed = PacedFeed::attach(&handoff);
    for k in 1..=3 {
        clk.set(b(k));
        handoff.offer(job(b(k)));
        drain(&handoff, &mut consumer, b(k));
    }
    drop(feed);

    assert!(
        queued(&bus).is_empty(),
        "off program: nothing for SP-program"
    );
    assert_eq!(
        handoff.snapshot().0.submitted,
        3,
        "its boundaries are still delivered (to the bus's progress record)"
    );
    assert_eq!(offer_to_bus(&bus, OFF_PROGRAM, job(b(4))), None);
    assert_eq!(
        offer_to_bus(&bus, ON_PROGRAM, job(b(4))),
        Some(OfferOutcome::Accepted),
        "the selected source's pair is taken"
    );
    assert_eq!(queued(&bus), vec![(b(4), 8)]);
}

/// A [`BoundaryOut`] whose delivery takes `cost_100ns` on the test's clock.
struct SlowOut {
    clk: SettableClock,
    cost_100ns: i64,
}

impl BoundaryOut for SlowOut {
    fn deliver(&mut self, _playlist_id: i64, _job: SubmitJob) {
        self.clk.set(self.clk.get() + self.cost_100ns);
    }
}

/// The consumer times each delivery on its own wall: its lateness from the
/// stamp to the START of the delivery, its cost from start to done (the
/// `iter_p99_us` of `merge_pacing_stats`), and the last delivery when it
/// ENDED. A delivery that takes 5 ms reads 5 000 µs.
#[test]
fn a_delivery_s_cost_is_its_time_on_the_consumer_s_wall() {
    let (_pacer_wall, clk) = WallClock::settable(0);
    let wall = WallClock::new(Box::new(clk.clone()));
    let handoff = SharedHandoff::new(SUBMIT_HANDOFF_BOUND);
    let out = SlowOut {
        clk: clk.clone(),
        cost_100ns: 50_000,
    };
    let mut consumer = PacedConsumer::new(out, ON_PROGRAM, wall, black());
    let feed = PacedFeed::attach(&handoff);
    for k in 1..=3 {
        clk.set(b(k));
        handoff.offer(job(b(k)));
        drain(&handoff, &mut consumer, b(k));
    }
    drop(feed);

    let c = handoff.snapshot().0;
    assert_eq!(c.submitted, 3);
    assert_eq!(c.submit_p99_us(), 5_000, "each delivery took 5 ms");
    assert_eq!(c.max_late_us, 0, "each delivery STARTED on its boundary");
    assert_eq!(c.late_frames, 0);
    assert_eq!(
        c.last_submit_100ns,
        b(3) + 50_000,
        "the last delivery is stamped when it ended"
    );
}
