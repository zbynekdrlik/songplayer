//! Linux tests for the pure #168 emit→submit handoff decision layer.

use super::*;
use crate::playback::ndi_health::PacingStats;

// ---- handoff_policy: bounded queue + coalesce-to-freshest ----

#[test]
fn offer_enqueues_below_bound() {
    let mut q: SubmitQueue<u32> = SubmitQueue::new(SUBMIT_HANDOFF_BOUND);
    assert_eq!(SUBMIT_HANDOFF_BOUND, 2, "box-test-5 depth: 2");
    assert_eq!(q.offer(1), HandoffOutcome::Enqueued { depth: 1 });
    assert_eq!(q.offer(2), HandoffOutcome::Enqueued { depth: 2 });
    assert!(q.is_full());
    assert_eq!(q.depth(), 2);
}

#[test]
fn offer_coalesces_oldest_when_full() {
    // At the bound, the submit thread is >= bound slots behind. The stalest
    // queued job (1) is dropped and the freshest (3) takes its place, keeping the
    // two NEWEST — the frames closest to live.
    let mut q: SubmitQueue<u32> = SubmitQueue::new(2);
    q.offer(1);
    q.offer(2);
    assert_eq!(q.offer(3), HandoffOutcome::Coalesced { depth: 2 });
    // FIFO of the survivors: 2 then 3 (1 was dropped, never 2 or 3).
    assert_eq!(q.take(), Some(2));
    assert_eq!(q.take(), Some(3));
    assert_eq!(q.take(), None);
}

#[test]
fn take_is_fifo_and_drains_to_empty() {
    let mut q: SubmitQueue<u32> = SubmitQueue::new(2);
    q.offer(10);
    q.offer(20);
    assert_eq!(q.take(), Some(10));
    assert!(!q.is_empty());
    assert_eq!(q.take(), Some(20));
    assert!(q.is_empty());
    assert_eq!(q.take(), None);
}

#[test]
#[should_panic(expected = "handoff bound must be >= 1")]
fn zero_bound_panics() {
    let _q: SubmitQueue<u32> = SubmitQueue::new(0);
}

// ---- honest submit-side lateness (measured at the submit thread) ----

#[test]
fn submit_late_floors_at_zero_and_measures_from_stamp() {
    // Frame submitted BEFORE its stamp (impossible on a monotonic clock, but the
    // math must floor): 0.
    assert_eq!(submit_late_100ns(1_000, 500), 0);
    // Frame that began leaving the box 600 units (60 µs) after its stamp.
    assert_eq!(submit_late_100ns(1_000, 1_600), 600);
    // Exactly on time: 0.
    assert_eq!(submit_late_100ns(1_000, 1_000), 0);
}

#[test]
fn is_submit_late_uses_2ms_threshold() {
    // The 2 ms floor is 20_000 (100-ns units). Strictly greater counts as late.
    assert!(!is_submit_late(0));
    assert!(!is_submit_late(20_000)); // exactly 2 ms is NOT late (strict >)
    assert!(is_submit_late(20_001));
    // A realistic 5 ms-late submit MUST count as late (the box-test-5 regime).
    assert!(is_submit_late(50_000));
    // A ~90 ms p99 stall is unambiguously late.
    assert!(is_submit_late(900_000));
}

// ---- submit counters ----

#[test]
fn record_submit_counts_late_cost_and_last_ts() {
    let mut c = SubmitCounters::new();
    // On-time submit (0.5 ms late < 2 ms): counted, NOT late.
    c.record_submit(5_000, 250_000, 10_000_000);
    assert_eq!(c.submitted, 1);
    assert_eq!(c.late_frames, 0);
    // Late submit (5 ms late = 50_000): counted AND late; max_late_us = 5000 µs.
    c.record_submit(50_000, 260_000, 10_000_400);
    assert_eq!(c.submitted, 2);
    assert_eq!(c.late_frames, 1);
    assert_eq!(c.max_late_us, 5_000);
    // last_submit tracks the most recent submit-done wall clock.
    assert_eq!(c.last_submit_100ns, 10_000_400);
}

#[test]
fn record_drop_bumps_dropped_only() {
    let mut c = SubmitCounters::new();
    c.record_drop();
    c.record_drop();
    assert_eq!(c.dropped, 2);
    assert_eq!(c.submitted, 0);
    assert_eq!(c.late_frames, 0);
}

#[test]
fn submit_p99_reports_worst_cost_us() {
    let mut c = SubmitCounters::new();
    // 100 cheap submits (250 µs) + 1 spike (90 ms). p99 index = 99*101/100 = 99,
    // which lands on the spike after sort.
    for _ in 0..100 {
        c.record_submit(0, 2_500, 0); // 2_500 * 100ns = 250 µs
    }
    c.record_submit(0, 900_000, 0); // 900_000 * 100ns = 90_000 µs
    assert_eq!(c.submit_p99_us(), 90_000);
    assert_eq!(c.submitted, 101);
}

#[test]
fn submit_p99_zero_when_empty() {
    let c = SubmitCounters::new();
    assert_eq!(c.submit_p99_us(), 0);
}

// ---- merge_pacing_stats: honest split of ownership ----

#[test]
fn merge_takes_late_from_submit_and_schedule_from_pacer() {
    // The pacer's emit-side late is now ~meaningless (~0), and its counters carry
    // the scheduling truth; the submit thread carries the honest output truth.
    let pacer = PacingStats {
        enabled: true,
        seq: 1000,
        late_frames: 0, // emit thread was on time
        max_late_us: 0,
        jitter_p99_us: 7,
        repeats: 12,
        resyncs: 1,
        relatches: 2,
        dropped: 3, // decode-decimation drops
        lag_slots: 4,
        iter_p99_us: 0,
        prep_p99_us: 400,
    };
    let mut submit = SubmitCounters::new();
    submit.record_submit(50_000, 300_000, 0); // 1 late, cost 30_000 µs
    submit.record_drop(); // 1 handoff-coalesce drop
    let merged = merge_pacing_stats(pacer, &submit);

    // Honest output-side fields come from the submit thread.
    assert_eq!(merged.late_frames, 1);
    assert_eq!(merged.max_late_us, 5_000);
    assert_eq!(merged.iter_p99_us, 30_000);
    // dropped SUMS both kinds: 3 (decimation) + 1 (coalesce) = 4.
    assert_eq!(merged.dropped, 4);
    // Scheduling fields stay the pacer's.
    assert_eq!(merged.seq, 1000);
    assert_eq!(merged.repeats, 12);
    assert_eq!(merged.resyncs, 1);
    assert_eq!(merged.relatches, 2);
    assert_eq!(merged.lag_slots, 4);
    assert_eq!(merged.jitter_p99_us, 7);
    assert_eq!(merged.prep_p99_us, 400);
    assert!(merged.enabled);
}

// ---- SubmitJob stamp deadline ----

#[test]
fn submit_job_stamp_boundary_is_the_video_tc() {
    let job = SubmitJob {
        width: 1920,
        height: 1080,
        stride: 1920,
        video: vec![0u8; 8],
        audio: Vec::new(),
        video_tc_100ns: 3_333_300,
        audio_tc_100ns: 3_333_311,
    };
    assert_eq!(job.stamp_boundary_100ns(), 3_333_300);
}
