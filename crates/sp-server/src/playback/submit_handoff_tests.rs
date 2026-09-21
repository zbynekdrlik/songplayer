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
    // 99 cheap submits (250 µs) + 1 spike (90 ms) = 100 samples, so the spike is
    // the top 1 %. p99 index = (100 * 99) / 100 = 99 → the sorted ring is
    // [250 ×99 (idx 0..=98), 90000 (idx 99)], so v[99] == 90000: the spike is
    // reported. (A 1-in-101 spike would sit ABOVE the p99 rank and NOT be
    // reported — the earlier off-by-one this fixes.)
    for _ in 0..99 {
        c.record_submit(0, 2_500, 0); // 2_500 * 100ns = 250 µs
    }
    c.record_submit(0, 900_000, 0); // 900_000 * 100ns = 90_000 µs
    assert_eq!(c.submit_p99_us(), 90_000);
    assert_eq!(c.submitted, 100);
}

#[test]
fn percentile_rank_is_nearest_rank_not_the_last_sample() {
    // 200 distinct costs 1..=200 µs: the p99 rank is (200 * 99) / 100 = 198 →
    // the sorted value 199 µs — NOT the last sample (200 µs). Pins the `/ 100`
    // rank arithmetic (a `*` there would clamp to the last sample).
    let mut c = SubmitCounters::new();
    for us in 1..=200u64 {
        c.record_submit(0, (us * 10) as i64, 0); // µs → 100 ns units
    }
    assert_eq!(c.cost_percentile_us(99), 199);
    assert_eq!(c.cost_percentile_us(50), 101);
}

#[test]
fn percentile_100_clamps_to_the_last_sample_without_panicking() {
    // p = 100 → raw rank == len, which the `.min(len - 1)` clamp pulls back to
    // the last (max) sample instead of indexing out of bounds.
    let mut c = SubmitCounters::new();
    for us in [7u64, 3, 11] {
        c.record_submit(0, (us * 10) as i64, 0);
    }
    assert_eq!(c.cost_percentile_us(100), 11);
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
        ..Default::default()
    };
    let mut submit = SubmitCounters::new();
    submit.record_submit(50_000, 300_000, 0); // 1 late, cost 30_000 µs
    submit.record_drop(); // 1 handoff-coalesce drop
    // #168 r2: the paced submit-call gauge (worst send_video_async max/p99 µs).
    let paced_submit = PacedSubmitStats {
        submit_call_us_max: 90_000,
        submit_call_us_p99: 75_000,
    };
    let merged = merge_pacing_stats(pacer, &submit, paced_submit);

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
    // #168 r2: the submit-call gauge is carried straight from the paced submit
    // thread's drained window (max and p99 are NOT swapped).
    assert_eq!(merged.submit_call_us_max, 90_000);
    assert_eq!(merged.submit_call_us_p99, 75_000);
}

// ---- paced_submit_snapshot: worst-of fold over a heartbeat window (#168 r2) ----

#[test]
fn paced_submit_snapshot_keeps_the_worst_of_each() {
    // The submit thread drains `FrameSubmitter.submit_times` on its ~1 s
    // connection-poll cadence and folds each `(max, p99)` sub-window into the
    // per-heartbeat gauge, keeping the WORST of each so a spike is never diluted
    // by a following quiet sub-window (the heartbeat resets it on read).
    let start = PacedSubmitStats::default();
    // A big spike sub-window, then a quiet one: the worst must survive both.
    let after_spike = paced_submit_snapshot(start, 90_000, 75_000);
    assert_eq!(after_spike.submit_call_us_max, 90_000);
    assert_eq!(after_spike.submit_call_us_p99, 75_000);
    let after_quiet = paced_submit_snapshot(after_spike, 10, 5);
    assert_eq!(
        after_quiet.submit_call_us_max, 90_000,
        "a later quiet sub-window must NOT lower the window max"
    );
    assert_eq!(
        after_quiet.submit_call_us_p99, 75_000,
        "a later quiet sub-window must NOT lower the window p99"
    );
    // A yet bigger spike raises it.
    let after_bigger = paced_submit_snapshot(after_quiet, 954_000, 120_000);
    assert_eq!(after_bigger.submit_call_us_max, 954_000);
    assert_eq!(after_bigger.submit_call_us_p99, 120_000);
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
