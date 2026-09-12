//! Boundary-paced emission scheduler tests (#147).
//!
//! Drive the pure [`Pacer`] with a deterministic (injected) wall clock, a
//! synthetic PTS stream and a recording sink — no MediaFoundation, so the full
//! emit/repeat/drop/catch-up/resync/re-latch behaviour runs on the Linux CI
//! job. `super::*` resolves to the `pacer` module under test.

use super::*;
use crate::playback::wallclock::WallClock;
use sp_ndi::AudioFrame;

/// 30 fps pacing interval in ns.
const I30: i64 = 33_333_333;
/// One 100-ns stamp-grid slot at 30 fps.
const SLOT_100NS: i64 = 333_333;

fn mk_frame(pts_ns: i64) -> PacedFrame {
    PacedFrame {
        pts_ns,
        width: 4,
        height: 2,
        stride: 4,
        video: vec![0u8; 12],
        audio: vec![],
    }
}

fn mk_frame_with_audio(pts_ns: i64) -> PacedFrame {
    PacedFrame {
        pts_ns,
        width: 4,
        height: 2,
        stride: 4,
        video: vec![0u8; 12],
        audio: vec![AudioFrame {
            data: vec![0.1, 0.2, 0.3, 0.4],
            channels: 2,
            sample_rate: 48_000,
            timecode_100ns: None,
        }],
    }
}

/// A recording sink: captures the per-emit (video_tc, audio_tc).
#[derive(Default)]
struct RecordingSink {
    video_tcs: Vec<i64>,
    audio_tcs: Vec<i64>,
}

impl PacedSink for RecordingSink {
    fn emit(&mut self, _frame: &PacedFrame, video_tc_100ns: i64, audio_tc_100ns: i64) {
        self.video_tcs.push(video_tc_100ns);
        self.audio_tcs.push(audio_tc_100ns);
    }
}

fn new_pacer() -> Pacer {
    Pacer::with_wallclock(30, true, WallClock::fixed(0))
}

#[test]
fn exactly_one_emit_per_boundary_over_100_boundaries() {
    let mut pacer = new_pacer();
    pacer.anchor(0); // wall_start = I30
    let pts_list: Vec<i64> = (0..120).map(|j| j * I30).collect();
    let mut idx = 0usize;
    let mut sink = RecordingSink::default();

    let mut emits = 0;
    for k in 1..=100i64 {
        let now = k * I30;
        let outcome = pacer.service(
            now,
            || {
                if idx < pts_list.len() {
                    let f = mk_frame(pts_list[idx]);
                    idx += 1;
                    Some(f)
                } else {
                    None
                }
            },
            &mut sink,
        );
        if outcome == ServiceOutcome::Emitted {
            emits += 1;
        }
    }

    assert_eq!(emits, 100, "exactly one emit per boundary");
    assert_eq!(pacer.stats().seq, 100);
    assert_eq!(
        pacer.stats().dropped,
        0,
        "one-frame-per-boundary drops nothing"
    );
    // Timecodes strictly increasing, each by exactly one grid slot.
    assert_eq!(sink.video_tcs.len(), 100);
    for w in sink.video_tcs.windows(2) {
        let step = w[1] - w[0];
        assert!(step == SLOT_100NS || step == SLOT_100NS + 1, "step={step}");
    }
}

#[test]
fn sixty_fps_input_decimates_to_thirty_with_dropped() {
    let mut pacer = new_pacer();
    pacer.anchor(0);
    // 60 fps frames: pts every 16_666_666 ns.
    let cap = 16_666_666i64;
    let pts_list: Vec<i64> = (0..120).map(|j| j * cap).collect();
    let mut idx = 0usize;
    let mut sink = RecordingSink::default();

    let mut emits = 0;
    for k in 1..=30i64 {
        let now = k * I30;
        let outcome = pacer.service(
            now,
            || {
                if idx < pts_list.len() {
                    let f = mk_frame(pts_list[idx]);
                    idx += 1;
                    Some(f)
                } else {
                    None
                }
            },
            &mut sink,
        );
        if outcome == ServiceOutcome::Emitted {
            emits += 1;
        }
    }

    assert!(
        (29..=31).contains(&emits),
        "60->30 decimation: {emits} emits"
    );
    let dropped = pacer.stats().dropped;
    assert!((26..=32).contains(&dropped), "~30/s dropped: {dropped}");
}

#[test]
fn sub_grid_input_repeats_with_strictly_increasing_stamps() {
    let mut pacer = new_pacer();
    pacer.anchor(0);
    // 23.976 fps frames: interval ~41_708_375 ns (> I30 -> some boundaries repeat).
    let cap = 41_708_375i64;
    let pts_list: Vec<i64> = (0..40).map(|j| j * cap).collect();
    let mut idx = 0usize;
    let mut sink = RecordingSink::default();

    for k in 1..=30i64 {
        let now = k * I30;
        pacer.service(
            now,
            || {
                if idx < pts_list.len() {
                    let f = mk_frame(pts_list[idx]);
                    idx += 1;
                    Some(f)
                } else {
                    None
                }
            },
            &mut sink,
        );
    }

    assert!(pacer.stats().repeats > 0, "sub-grid content must repeat");
    // Every stamp (emit + repeat) strictly increases by exactly one grid slot.
    assert_eq!(sink.video_tcs.len(), 30);
    for w in sink.video_tcs.windows(2) {
        let step = w[1] - w[0];
        assert!(step == SLOT_100NS || step == SLOT_100NS + 1, "step={step}");
    }
}

#[test]
fn underrun_repeats_last_frame_stamped_at_new_boundary() {
    let mut pacer = new_pacer();
    pacer.anchor(0); // wall_start = I30
    let mut sink = RecordingSink::default();

    // Emit one real frame at the first boundary.
    let mut first = Some(mk_frame(0));
    let o1 = pacer.service(I30, || first.take(), &mut sink);
    assert_eq!(o1, ServiceOutcome::Emitted);

    // Next boundary: no new frame -> repeat the last, stamped at THIS boundary.
    let o2 = pacer.service(2 * I30, || None, &mut sink);
    assert_eq!(o2, ServiceOutcome::Repeated);
    assert_eq!(pacer.stats().repeats, 1);
    assert_eq!(sink.video_tcs.len(), 2);
    assert!(
        sink.video_tcs[1] > sink.video_tcs[0],
        "repeat stamp must advance to the new boundary"
    );
}

#[test]
fn late_by_three_intervals_catches_up_then_on_time() {
    let mut pacer = new_pacer();
    pacer.anchor(0); // wall_start = I30, next boundary = I30
    // Frames are buffered (available) for each boundary.
    let pts_list: Vec<i64> = (0..10).map(|j| j * I30).collect();
    let mut idx = 0usize;
    let mut sink = RecordingSink::default();

    // The thread stalled: `now` is 3 intervals past the first boundary. Service
    // repeatedly at the SAME now — the pacer catches up one interval per emit.
    let now = I30 + 3 * I30; // B0 + 3 intervals
    for _ in 0..4 {
        pacer.service(
            now,
            || {
                if idx < pts_list.len() {
                    let f = mk_frame(pts_list[idx]);
                    idx += 1;
                    Some(f)
                } else {
                    None
                }
            },
            &mut sink,
        );
    }

    // 3 late catch-up emits, then an on-time emit; the 5th call would Wait.
    assert_eq!(
        pacer.stats().late_frames,
        3,
        "three catch-up emits are late"
    );
    assert_eq!(pacer.stats().resyncs, 0, "a bounded catch-up never resyncs");
    assert_eq!(pacer.stats().seq, 4);
    let wait = pacer.service(now, || None, &mut sink);
    assert!(
        matches!(wait, ServiceOutcome::Wait { .. }),
        "caught up -> Wait"
    );
}

#[test]
fn late_by_twelve_with_nothing_pending_resyncs() {
    let mut pacer = new_pacer();
    pacer.anchor(0); // wall_start = I30
    let mut sink = RecordingSink::default();

    // Emit one frame so there is a last frame to repeat.
    let mut first = Some(mk_frame(0));
    pacer.service(I30, || first.take(), &mut sink);
    assert_eq!(pacer.stats().resyncs, 0);

    // Long stall, NO frames available (underrun): now jumps ~13 intervals.
    let now = I30 + 13 * I30;
    let o = pacer.service(now, || None, &mut sink);
    assert_eq!(o, ServiceOutcome::Repeated);
    assert_eq!(
        pacer.stats().resyncs,
        1,
        "lag>8 with nothing buffered resyncs"
    );
}

#[test]
fn backward_clock_step_relatches() {
    let mut pacer = new_pacer();
    let anchor_now = 100 * I30;
    pacer.anchor(anchor_now); // wall_start = 101*I30
    let mut sink = RecordingSink::default();

    let mut first = Some(mk_frame(0));
    pacer.service(101 * I30, || first.take(), &mut sink);
    assert_eq!(pacer.stats().relatches, 0);

    // Clock steps backward far below the latched boundary.
    let o = pacer.service(50 * I30, || None, &mut sink);
    assert!(matches!(o, ServiceOutcome::Wait { .. }));
    assert_eq!(
        pacer.stats().relatches,
        1,
        "a backward step re-latches once"
    );
}

#[test]
fn audio_is_submitted_before_video_at_every_boundary() {
    use sp_ndi::MockNdiBackend;
    use sp_ndi::NdiSender;
    use std::sync::Arc;

    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "Paced", false, false).unwrap();
    let mut submitter = crate::playback::submitter::FrameSubmitter::new(sender, 30, 1);

    let mut pacer = new_pacer();
    pacer.anchor(0);
    let pts_list: Vec<i64> = (0..30).map(|j| j * I30).collect();
    let mut idx = 0usize;

    for k in 1..=10i64 {
        let now = k * I30;
        pacer.service(
            now,
            || {
                if idx < pts_list.len() {
                    let f = mk_frame_with_audio(pts_list[idx]);
                    idx += 1;
                    Some(f)
                } else {
                    None
                }
            },
            &mut submitter,
        );
    }

    let calls = backend.calls();
    // For every send_video_async there must be a preceding send_audio since the
    // last video send (audio-before-video invariant).
    let mut saw_audio_since_video = false;
    let mut video_count = 0;
    for c in &calls {
        if c.starts_with("send_audio") {
            saw_audio_since_video = true;
        } else if c.starts_with("send_video_async") {
            assert!(
                saw_audio_since_video,
                "audio must precede each paced video frame: {calls:#?}"
            );
            saw_audio_since_video = false;
            video_count += 1;
        }
    }
    assert_eq!(video_count, 10, "one paced video frame per boundary");
    // Video timecodes strictly increasing (each frame its own grid slot).
    let tcs = backend.video_timecodes();
    for w in tcs.windows(2) {
        assert!(w[1] > w[0], "paced video timecodes must strictly increase");
    }
}

#[test]
fn jitter_p99_is_computed_from_the_ring() {
    let mut pacer = new_pacer();
    pacer.anchor(0);
    let pts_list: Vec<i64> = (0..30).map(|j| j * I30).collect();
    let mut idx = 0usize;
    let mut sink = RecordingSink::default();

    // Service each boundary a fixed 7 µs late -> every jitter sample is 7 µs.
    for k in 1..=20i64 {
        let now = k * I30 + 7_000; // 7 µs past the boundary
        pacer.service(
            now,
            || {
                if idx < pts_list.len() {
                    let f = mk_frame(pts_list[idx]);
                    idx += 1;
                    Some(f)
                } else {
                    None
                }
            },
            &mut sink,
        );
    }

    let stats = pacer.stats();
    assert_eq!(stats.jitter_p99_us, 7, "p99 of a constant 7 µs jitter is 7");
    assert_eq!(stats.late_frames, 0, "7 µs is well under one interval");
    assert!(stats.max_late_us >= 7);
}

#[test]
fn counters_serialise_on_the_snapshot() {
    let mut pacer = new_pacer();
    pacer.anchor(0);
    let mut first = Some(mk_frame(0));
    pacer.service(I30, || first.take(), &mut RecordingSink::default());
    pacer.service(2 * I30, || None, &mut RecordingSink::default()); // a repeat

    let stats = pacer.stats();
    assert!(stats.enabled);
    let json = serde_json::to_value(&stats).unwrap();
    for field in [
        "enabled",
        "seq",
        "late_frames",
        "max_late_us",
        "jitter_p99_us",
        "repeats",
        "resyncs",
        "relatches",
        "dropped",
    ] {
        assert!(json.get(field).is_some(), "missing pacing field {field}");
    }
    assert_eq!(json["repeats"].as_u64(), Some(1));
    // serde round-trip.
    let back: crate::playback::ndi_health::PacingStats = serde_json::from_value(json).unwrap();
    assert_eq!(back, stats);
}

#[test]
fn disabled_pacer_reports_default_stats() {
    let pacer = Pacer::with_wallclock(30, false, WallClock::fixed(0));
    let stats = pacer.stats();
    assert!(!stats.enabled);
    assert_eq!(stats, crate::playback::ndi_health::PacingStats::default());
}

// ---------------------------------------------------------------------------
// Structural guard: the flag-OFF path is byte-preserved in the pipeline source.
// ---------------------------------------------------------------------------

#[test]
fn genlock_pacing_off_keeps_the_legacy_sdk_clocked_path() {
    // CRLF-normalise so the grep is line-ending agnostic.
    let src = include_str!("pipeline.rs").replace("\r\n", "\n");
    assert!(
        src.contains("new_with_clocking(backend, ndi_name, true, false)"),
        "flag-OFF must still create the SDK-clocked sender (clock_video=true)"
    );
    assert!(
        src.contains("new_with_clocking(backend, ndi_name, false, false)"),
        "flag-ON must create the app-clocked sender (clock_video=false)"
    );
    // The legacy decode path still applies the per-file frame rate.
    let submitter_src = include_str!("submitter.rs").replace("\r\n", "\n");
    let pipeline_src = &src;
    assert!(
        pipeline_src.contains("set_frame_rate") || submitter_src.contains("set_frame_rate"),
        "the legacy path still calls set_frame_rate from the decoder"
    );
}
