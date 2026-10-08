//! #147: the per-boundary trace (`program_trace.rs`): the ring and its
//! seqlock, the record's fields, the song mark, the query window, the clump
//! detector and the minute summary. Exact pins, derived with a scratch
//! Python model of the module (rust-workspace.md, no-compile box).
//! Wired via `#[cfg(test)] #[path = "program_trace_tests.rs"] mod tests;`.

use std::sync::Arc;

use sp_core::genlock::{GENLOCK_GRID_FPS, grid_boundary_100ns};

use super::*;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_bus::ProgramJob;
use crate::playback::program_output_timing::BoundaryMarks;
use crate::playback::program_transition::MixJob;
use crate::playback::submit_handoff::SubmitJob;

/// Grid index of 2025-10-08 00:13:20 UTC (an exact second).
const G0: i64 = 1_759_882_400 * GENLOCK_GRID_FPS;
const A: i64 = 11;
const B: i64 = 22;
const C: i64 = 33;

/// The `k`-th grid boundary after `G0`'s.
fn b(k: i64) -> i64 {
    grid_boundary_100ns(G0 + k, GENLOCK_GRID_FPS)
}

/// The marks of boundary `stamp` with its four other instants `us` µs after
/// it (taken, fed, video side started, submitted).
fn marks(stamp: i64, us: [i64; 4]) -> BoundaryMarks {
    BoundaryMarks {
        stamp_100ns: stamp,
        taken_100ns: stamp + us[0] * 10,
        fed_100ns: stamp + us[1] * 10,
        submit_start_100ns: stamp + us[2] * 10,
        submitted_100ns: stamp + us[3] * 10,
    }
}

const fn shape(kind: TraceKind, live: bool) -> JobShape {
    JobShape { kind, live }
}

const SRC: JobShape = shape(TraceKind::Source, true);
const SRC_STANDBY: JobShape = shape(TraceKind::Source, false);
const FILL: JobShape = shape(TraceKind::Fill, false);
const FADE: JobShape = shape(TraceKind::Fade, true);

/// Boundary `k` served on time (1 ms, 1.2 ms, 1.5 ms, 4 ms after it), as
/// `source` with `shape`, under K = 0.
fn on_time(w: &mut TraceWriter, k: i64, source: Option<i64>, shape: JobShape) {
    w.record(&marks(b(k), [1_000, 1_200, 1_500, 4_000]), source, shape, 0);
}

fn trace(capacity: usize) -> (Arc<ProgramTrace>, TraceWriter) {
    let trace = Arc::new(ProgramTrace::with_capacity(capacity));
    let writer = trace.writer().expect("the first writer");
    (trace, writer)
}

/// A record as the detector reads it: its boundary, how late its job was
/// taken, when its submit returned (µs after the boundary).
fn rec(index: u64, stamp: i64, taken_us: i64, submitted_us: i64) -> TraceRecord {
    TraceRecord {
        index,
        stamp_100ns: stamp,
        wire_100ns: stamp,
        utc_ms: stamp / 10_000,
        taken_us,
        fed_us: taken_us,
        submit_start_us: taken_us,
        submitted_us,
        source: Some(A),
        kind: TraceKind::Source,
        live: true,
        song: None,
    }
}

// ---- the ring ----

/// Every field comes back as written: both signs of every offset, a source
/// and a song present and absent, each kind, live and not.
#[test]
fn a_record_reads_back_whole() {
    let t = ProgramTrace::with_capacity(8);
    let records = [
        TraceRecord {
            index: 0,
            stamp_100ns: b(0),
            wire_100ns: b(3),
            utc_ms: 17_598_824_001_234,
            taken_us: -400,
            fed_us: 1_234,
            submit_start_us: 2_345,
            submitted_us: 98_765,
            source: Some(-1),
            kind: TraceKind::Cut,
            live: true,
            song: Some(4_242),
        },
        TraceRecord {
            index: 1,
            stamp_100ns: b(1),
            wire_100ns: b(1),
            utc_ms: -5,
            taken_us: 33_334,
            fed_us: -7,
            submit_start_us: -8,
            submitted_us: -9,
            source: None,
            kind: TraceKind::Fill,
            live: false,
            song: None,
        },
        TraceRecord {
            index: 2,
            kind: TraceKind::Fade,
            source: Some(B),
            ..rec(2, b(2), 5, 6)
        },
        rec(3, b(3), 7, 8),
    ];
    for r in &records {
        t.push(r);
    }
    for (i, r) in records.iter().enumerate() {
        assert_eq!(t.read(i as u64), Some(*r), "record {i}");
    }
    assert_eq!(t.written(), 4);
}

#[test]
fn an_empty_trace_reads_nothing() {
    let t = ProgramTrace::with_capacity(4);
    assert_eq!(t.read(0), None);
    assert_eq!((t.written(), t.held()), (0, 0));
    assert_eq!(t.oldest(), None);
    assert_eq!(t.newest(), None);
    assert!(t.records_from(0).is_empty());
}

/// Four slots, six records: the last four are held, oldest first.
#[test]
fn the_ring_keeps_the_last_capacity_records() {
    let (t, mut w) = trace(4);
    for k in 0..6 {
        on_time(&mut w, k, Some(A), SRC);
    }
    assert_eq!((t.capacity(), t.written(), t.held()), (4, 6, 4));
    assert_eq!(t.read(0), None);
    assert_eq!(t.read(1), None);
    for i in 2..6 {
        let r = t.read(i).expect("held");
        assert_eq!((r.index, r.stamp_100ns), (i, b(i as i64)));
    }
    assert_eq!(t.read(6), None, "not written yet");
    let indices = |v: Vec<TraceRecord>| v.iter().map(|r| r.index).collect::<Vec<_>>();
    assert_eq!(indices(t.records_from(0)), [2, 3, 4, 5]);
    assert_eq!(indices(t.records_from(4)), [4, 5]);
    assert_eq!(t.oldest().map(|r| r.index), Some(2));
    assert_eq!(t.newest().map(|r| r.index), Some(5));
}

/// A held count below the capacity, and a ring of one.
#[test]
fn a_trace_of_one_holds_the_newest_record() {
    let (t, mut w) = trace(1);
    on_time(&mut w, 0, Some(A), SRC);
    assert_eq!(t.held(), 1);
    on_time(&mut w, 1, Some(A), SRC);
    assert_eq!((t.capacity(), t.written(), t.held()), (1, 2, 1));
    assert_eq!(t.read(0), None);
    assert_eq!(t.read(1).map(|r| r.stamp_100ns), Some(b(1)));
    let (t3, mut w3) = trace(3);
    on_time(&mut w3, 0, Some(A), SRC);
    on_time(&mut w3, 1, Some(A), SRC);
    assert_eq!((t3.written(), t3.held()), (2, 2));
}

/// The production trace holds 10 minutes of the 30 fps grid.
#[test]
fn the_default_trace_holds_ten_minutes() {
    assert_eq!(TRACE_CAPACITY as i64, 10 * 60 * GENLOCK_GRID_FPS);
    assert_eq!(ProgramTrace::default().capacity(), 18_000);
}

#[test]
#[should_panic(expected = "a trace holds at least one record")]
fn a_trace_of_no_records_is_refused() {
    let _ = ProgramTrace::with_capacity(0);
}

/// A slot the writer is filling reads as nothing; once it is closed, whole.
#[test]
fn a_slot_being_written_reads_as_nothing() {
    let slot = Slot::new();
    let seq = slot.open();
    slot.fill([7; WORDS]);
    assert_eq!(slot.load_with(|| {}), None, "the sequence is odd");
    slot.close(seq);
    assert_eq!(slot.load_with(|| {}), Some([7; WORDS]));
    let again = slot.open();
    assert_eq!(slot.load_with(|| {}), None, "the second record too");
    slot.close(again);
    assert!(slot.load_with(|| {}).is_some());
}

/// A read the writer overtook (the slot written again while its words were
/// read) is dropped, though every word it read was the old record's.
#[test]
fn a_read_the_writer_overtook_is_dropped() {
    let (t, mut w) = trace(1);
    on_time(&mut w, 0, Some(A), SRC);
    let overtaken = t.read_with(0, || on_time(&mut w, 1, Some(A), SRC));
    assert_eq!(overtaken, None);
    assert_eq!(t.read(1).map(|r| r.index), Some(1));
    let (t2, mut w2) = trace(2);
    on_time(&mut w2, 0, Some(A), SRC);
    let untouched = t2.read_with(0, || on_time(&mut w2, 1, Some(A), SRC));
    assert_eq!(
        untouched.map(|r| r.index),
        Some(0),
        "another slot was written"
    );
}

/// One writer at a time; a dropped writer frees the trace for the next.
#[test]
fn the_trace_has_one_writer_at_a_time() {
    let t = Arc::new(ProgramTrace::with_capacity(2));
    let first = t.writer().expect("the first writer");
    assert!(t.writer().is_none(), "a second writer is refused");
    drop(first);
    assert!(t.writer().is_some(), "free again once dropped");
}

// ---- the record ----

/// The instants are µs after the boundary (a job taken before it is
/// negative), the wire stamp and the UTC under K = 0.
#[test]
fn a_record_holds_its_marks_as_us_after_its_boundary() {
    let (t, mut w) = trace(4);
    let m = BoundaryMarks {
        stamp_100ns: b(1),
        taken_100ns: b(1) - 4_005,
        fed_100ns: b(1) + 15_009,
        submit_start_100ns: b(1) + 20_000,
        submitted_100ns: b(1) + 73_001,
    };
    w.record(&m, Some(A), SRC, 0);
    let r = t.read(0).expect("written");
    assert_eq!(r.stamp_100ns, 17_598_824_000_333_333);
    assert_eq!(r.wire_100ns, r.stamp_100ns);
    assert_eq!(
        (r.taken_us, r.fed_us, r.submit_start_us, r.submitted_us),
        (-400, 1_500, 2_000, 7_300)
    );
    assert_eq!(r.utc_ms, 1_759_882_400_040);
    assert_eq!((r.source, r.live, r.song), (Some(A), true, None));
}

/// Under a fleet shift of K slots the wire stamp is the boundary K slots
/// later and the UTC of the submit return moves by D(K).
#[test]
fn under_a_fleet_shift_the_wire_stamp_and_the_utc_move_by_its_slots() {
    let (t, mut w) = trace(4);
    w.record(&marks(b(0), [0, 0, 0, 4_000]), Some(A), SRC, 3);
    w.record(&marks(b(1), [0, 0, 0, 4_000]), Some(A), SRC, -48);
    let shifted = t.read(0).expect("written");
    assert_eq!(shifted.wire_100ns, b(3));
    assert_eq!(shifted.utc_ms, 1_759_882_400_104);
    assert_eq!(shifted.submitted_us, 4_000, "the offsets do not move");
    let back = t.read(1).expect("written");
    assert_eq!(back.wire_100ns, b(1 - 48));
    assert_eq!(back.utc_ms, 1_759_882_398_437);
}

/// A forward of another source than the record before is a cut; a fill or
/// a fade keeps its kind, and the source it names counts for the next one.
#[test]
fn a_forward_of_another_source_is_a_cut() {
    let (t, mut w) = trace(16);
    let steps: [(Option<i64>, JobShape); 11] = [
        (Some(A), SRC),
        (Some(A), SRC),
        (Some(A), FILL),
        (Some(A), SRC_STANDBY),
        (Some(B), FADE),
        (Some(B), SRC),
        (Some(C), SRC),
        (None, FILL),
        (Some(C), SRC),
        (Some(C), FILL),
        (Some(B), FILL),
    ];
    for (k, (source, shape)) in steps.iter().enumerate() {
        on_time(&mut w, k as i64, *source, *shape);
    }
    let kinds: Vec<&str> = t.records_from(0).iter().map(|r| r.kind.label()).collect();
    assert_eq!(
        kinds,
        [
            "cut", "src", "fill", "src", "fade", "src", "cut", "fill", "cut", "fill", "fill"
        ]
    );
    let live: Vec<bool> = t.records_from(0).iter().map(|r| r.live).collect();
    assert_eq!(
        live,
        [
            true, true, false, false, true, true, true, false, true, false, false
        ]
    );
}

fn submit_job(live: bool) -> SubmitJob {
    SubmitJob {
        width: 2,
        height: 2,
        stride: 2,
        video: SharedFrame::new(vec![0; 6]),
        audio: Vec::new(),
        video_tc_100ns: b(0),
        audio_tc_100ns: b(0),
        live,
    }
}

fn mix(from: Option<SubmitJob>, to: Option<SubmitJob>) -> ProgramJob {
    ProgramJob::Mix(MixJob {
        stamp_100ns: b(0),
        from,
        to,
        slot: 0,
        n_slots: 9,
    })
}

/// What a job alone says: a forward and its pair's liveness, a fill, a fade
/// live by its incoming pair.
#[test]
fn a_job_s_shape_is_its_kind_and_whether_its_pair_is_live() {
    let cases = [
        (ProgramJob::Source(submit_job(true)), SRC),
        (ProgramJob::Source(submit_job(false)), SRC_STANDBY),
        (ProgramJob::Standby { stamp_100ns: b(0) }, FILL),
        (mix(Some(submit_job(false)), Some(submit_job(true))), FADE),
        (
            mix(Some(submit_job(true)), Some(submit_job(false))),
            shape(TraceKind::Fade, false),
        ),
        (
            mix(Some(submit_job(true)), None),
            shape(TraceKind::Fade, false),
        ),
    ];
    for (i, (job, want)) in cases.iter().enumerate() {
        assert_eq!(JobShape::of(job), *want, "case {i}");
    }
}

#[test]
fn every_kind_has_its_label() {
    let labels = [
        TraceKind::Source,
        TraceKind::Cut,
        TraceKind::Fill,
        TraceKind::Fade,
    ]
    .map(TraceKind::label);
    assert_eq!(labels, ["src", "cut", "fill", "fade"]);
}

// ---- the song mark ----

/// A song mark goes on its source's next LIVE boundary, once: never on
/// another source's, never on a standby pair.
#[test]
fn a_song_mark_goes_on_its_source_s_first_live_boundary() {
    let (t, mut w) = trace(16);
    on_time(&mut w, 0, Some(A), SRC);
    t.mark_song(A, 42);
    on_time(&mut w, 1, Some(B), SRC);
    on_time(&mut w, 2, Some(A), SRC_STANDBY);
    on_time(&mut w, 3, Some(A), FILL);
    on_time(&mut w, 4, Some(A), SRC);
    on_time(&mut w, 5, Some(A), SRC);
    let songs: Vec<Option<i64>> = t.records_from(0).iter().map(|r| r.song).collect();
    assert_eq!(songs, [None, None, None, None, Some(42), None]);
}

/// A fade's boundary is live by its incoming pair, so a song that starts
/// with a fade is marked on the first live one.
#[test]
fn a_song_that_starts_with_a_fade_is_marked_on_its_first_live_mix() {
    let (t, mut w) = trace(8);
    t.mark_song(B, 7);
    on_time(&mut w, 0, Some(B), shape(TraceKind::Fade, false));
    on_time(&mut w, 1, Some(B), FADE);
    let songs: Vec<Option<i64>> = t.records_from(0).iter().map(|r| r.song).collect();
    assert_eq!(songs, [None, Some(7)]);
}

#[test]
fn a_later_song_mark_replaces_the_earlier_one() {
    let (t, mut w) = trace(4);
    t.mark_song(A, 42);
    t.mark_song(A, 43);
    on_time(&mut w, 0, Some(A), SRC);
    assert_eq!(t.read(0).and_then(|r| r.song), Some(43));
}

/// A mark set after 5 records is taken 300 boundaries later, never 301:
/// that song's first live boundary went out without it.
#[test]
fn a_song_mark_is_dropped_after_300_boundaries() {
    for (gap, want) in [(300_u64, Some(42)), (301, None)] {
        let (t, mut w) = trace(1_000);
        for k in 0..5 {
            on_time(&mut w, k, Some(B), SRC);
        }
        t.mark_song(A, 42);
        for k in 0..gap {
            on_time(&mut w, 5 + k as i64, Some(B), SRC);
        }
        let index = 5 + gap;
        on_time(&mut w, index as i64, Some(A), SRC);
        let r = t.read(index).expect("written");
        assert_eq!(r.song, want, "{gap} boundaries after the mark");
        on_time(&mut w, index as i64 + 1, Some(A), SRC);
        assert_eq!(t.read(index + 1).and_then(|r| r.song), None);
    }
}

/// The writer never waits for the engine: while the mark is held, the
/// boundary goes out unmarked and the next one takes the mark.
#[test]
fn a_mark_the_engine_holds_goes_on_the_next_boundary() {
    let (t, mut w) = trace(4);
    t.mark_song(A, 42);
    let held = t.mark.lock().unwrap();
    on_time(&mut w, 0, Some(A), SRC);
    drop(held);
    on_time(&mut w, 1, Some(A), SRC);
    let songs: Vec<Option<i64>> = t.records_from(0).iter().map(|r| r.song).collect();
    assert_eq!(songs, [None, Some(42)]);
}

// ---- the query window ----

const NOW: i64 = 1_759_882_500_000;

#[test]
fn a_query_with_no_bounds_is_the_last_two_minutes() {
    let span = TraceSpan::resolve(None, None, NOW).unwrap();
    assert_eq!(
        span,
        TraceSpan {
            from_utc_ms: NOW - 120_000,
            to_utc_ms: NOW,
            clamped: false,
        }
    );
    let to_only = TraceSpan::resolve(None, Some(NOW - 5), NOW + 7).unwrap();
    assert_eq!(
        (to_only.from_utc_ms, to_only.to_utc_ms),
        (NOW - 120_005, NOW - 5)
    );
}

/// Two minutes exactly is answered as asked; one ms more is clamped to two
/// minutes after `from`, and says so.
#[test]
fn a_query_over_two_minutes_is_clamped_and_says_so() {
    let exact = TraceSpan::resolve(Some(NOW - 120_000), None, NOW).unwrap();
    assert_eq!((exact.to_utc_ms, exact.clamped), (NOW, false));
    let over = TraceSpan::resolve(Some(NOW - 120_001), None, NOW).unwrap();
    assert_eq!(
        over,
        TraceSpan {
            from_utc_ms: NOW - 120_001,
            to_utc_ms: NOW - 1,
            clamped: true,
        }
    );
    let far = TraceSpan::resolve(Some(1_000), Some(NOW), NOW).unwrap();
    assert_eq!((far.to_utc_ms, far.clamped), (121_000, true));
}

#[test]
fn a_query_whose_from_is_after_its_to_is_refused_an_empty_one_is_not() {
    assert_eq!(
        TraceSpan::resolve(Some(NOW + 1), Some(NOW), NOW),
        Err("from_utc_ms is after to_utc_ms")
    );
    let empty = TraceSpan::resolve(Some(NOW), Some(NOW), NOW).unwrap();
    assert_eq!((empty.from_utc_ms, empty.to_utc_ms), (NOW, NOW));
    assert!(!empty.contains(NOW));
}

/// The extremes of `i64` never overflow.
#[test]
fn a_query_at_the_ends_of_i64_never_overflows() {
    let all = TraceSpan::resolve(Some(i64::MIN), Some(i64::MAX), NOW).unwrap();
    assert_eq!(
        (all.from_utc_ms, all.to_utc_ms, all.clamped),
        (i64::MIN, i64::MIN + 120_000, true)
    );
    let low = TraceSpan::resolve(None, Some(i64::MIN), NOW).unwrap();
    assert_eq!((low.from_utc_ms, low.to_utc_ms), (i64::MIN, i64::MIN));
    let high = TraceSpan::resolve(Some(i64::MAX - 5), None, i64::MAX).unwrap();
    assert_eq!((high.to_utc_ms, high.clamped), (i64::MAX, false));
}

/// The window is `[from, to)`.
#[test]
fn the_window_holds_from_and_not_to() {
    let span = TraceSpan {
        from_utc_ms: 100,
        to_utc_ms: 200,
        clamped: false,
    };
    let inside: Vec<bool> = [99, 100, 199, 200].map(|ms| span.contains(ms)).into();
    assert_eq!(inside, [false, true, true, false]);
}

// ---- the clump detector ----

const S: i64 = 17_598_824_000_000_000;

/// Taken one slot (33 333 µs) late is not a clump; one µs more is.
#[test]
fn a_boundary_taken_over_a_slot_late_is_a_clump() {
    assert!(!ClumpFlags::of(None, &rec(0, S, 33_333, 40_000)).late);
    assert!(ClumpFlags::of(None, &rec(0, S, 33_334, 40_000)).late);
    assert!(!ClumpFlags::of(None, &rec(0, S, -50_000, 40_000)).late);
}

/// Submitted 10 ms after the boundary before is not a clump; 9.999 ms is.
/// The two boundaries' stamps are 33.333 ms apart, so the instants and not
/// the offsets decide.
#[test]
fn a_boundary_submitted_under_10_ms_after_the_one_before_is_a_clump() {
    let prev = rec(0, S, 0, 30_000);
    let at = |submitted_us| rec(1, S + 333_330, 0, submitted_us);
    assert!(ClumpFlags::of(Some(&prev), &at(6_666)).close, "9 999 µs");
    assert!(!ClumpFlags::of(Some(&prev), &at(6_667)).close, "10 000 µs");
    assert!(!ClumpFlags::of(Some(&prev), &at(40_000)).close, "36 666 µs");
    assert!(!ClumpFlags::of(None, &at(0)).close, "nothing before it");
    assert_eq!(at(6_666).submitted_at_100ns(), S + 399_990);
}

/// The scan holds each record against the one before; its first against the
/// record it was started after.
#[test]
fn the_scan_holds_each_record_against_the_one_before() {
    let first = rec(0, S, 0, 30_000);
    let second = rec(1, S + 333_333, 0, 1_000);
    let third = rec(2, S + 666_667, 40_000, 2_000);
    let mut scan = ClumpScan::default();
    assert_eq!(scan.next(&first), ClumpFlags::default());
    assert_eq!(
        scan.next(&second),
        ClumpFlags {
            late: false,
            close: true,
        }
    );
    assert_eq!(
        scan.next(&third),
        ClumpFlags {
            late: true,
            close: false,
        }
    );
    let mut after = ClumpScan::after(Some(first));
    assert!(after.next(&second).close);
}

// ---- the API answer ----

/// Five boundaries: on time, taken late, one submitted 1.3 ms after it
/// (with a song start), on time, a fill. A window over the last three reads
/// the late one before it for its first row's `close`.
fn five(w: &mut TraceWriter, t: &ProgramTrace) {
    on_time(w, 0, Some(A), SRC);
    w.record(
        &marks(b(1), [35_000, 35_100, 35_200, 38_000]),
        Some(A),
        SRC,
        0,
    );
    t.mark_song(A, 9);
    w.record(&marks(b(2), [500, 600, 700, 6_000]), Some(A), SRC, 0);
    on_time(w, 3, Some(A), SRC);
    w.record(&marks(b(4), [3_000, 3_100, 3_200, 3_900]), None, FILL, 0);
}

#[test]
fn the_answer_lists_the_window_s_rows_with_their_clump_flags() {
    let (t, mut w) = trace(16);
    five(&mut w, &t);
    let utc: Vec<i64> = t.records_from(0).iter().map(|r| r.utc_ms).collect();
    assert_eq!(
        utc,
        [
            1_759_882_400_004,
            1_759_882_400_071,
            1_759_882_400_072,
            1_759_882_400_104,
            1_759_882_400_137
        ]
    );
    let answer = TraceAnswer::build(
        &t,
        Some("1759882400072"),
        Some("1759882400138"),
        1_759_882_400_200,
    )
    .unwrap();
    assert_eq!(
        (answer.from_utc_ms, answer.to_utc_ms, answer.clamped),
        (1_759_882_400_072, 1_759_882_400_138, false)
    );
    assert_eq!(answer.max_span_ms, 120_000);
    assert_eq!((answer.capacity, answer.held), (16, 5));
    assert_eq!(
        (answer.oldest_utc_ms, answer.newest_utc_ms),
        (Some(1_759_882_400_004), Some(1_759_882_400_137))
    );
    assert_eq!(answer.columns, TRACE_COLUMNS);
    assert_eq!(
        answer.rows,
        [
            TraceRow(
                1_759_882_400_072,
                b(2),
                Some(A),
                "src",
                1,
                500,
                600,
                700,
                6_000,
                0,
                1,
                Some(9)
            ),
            TraceRow(
                1_759_882_400_104,
                b(3),
                Some(A),
                "src",
                1,
                1_000,
                1_200,
                1_500,
                4_000,
                0,
                0,
                None
            ),
            TraceRow(
                1_759_882_400_137,
                b(4),
                None,
                "fill",
                0,
                3_000,
                3_100,
                3_200,
                3_900,
                0,
                0,
                None
            ),
        ]
    );
    assert_eq!(
        answer.clumps,
        ClumpCounts {
            boundaries: 3,
            late: 0,
            close: 1,
            songs: 1,
        }
    );
    let late = TraceAnswer::build(&t, Some("1759882400071"), Some("1759882400072"), 0).unwrap();
    assert_eq!(late.rows.len(), 1);
    assert_eq!((late.clumps.late, late.clumps.close), (1, 0));
}

/// The window of the trace's first record has no record before it.
#[test]
fn the_first_record_of_the_trace_has_no_record_before_it() {
    let (t, mut w) = trace(4);
    w.record(&marks(b(0), [0, 0, 0, 100]), Some(A), SRC, 0);
    w.record(&marks(b(1), [0, 0, 0, 100]), Some(A), SRC, 0);
    let answer = TraceAnswer::build(&t, Some("0"), None, 1_759_882_400_100).unwrap();
    assert!(answer.clamped);
    assert_eq!(answer.rows.len(), 0, "two minutes after the epoch");
    let all = TraceAnswer::build(&t, None, None, 1_759_882_400_100).unwrap();
    let close: Vec<u8> = all.rows.iter().map(|r| r.10).collect();
    assert_eq!(close, [0, 0]);
}

/// A value that is not an integer, or a `from` after its `to`, is refused
/// with a fixed text that never repeats what was sent.
#[test]
fn a_query_that_is_not_two_integers_is_refused_without_echoing_it() {
    let t = ProgramTrace::with_capacity(2);
    let refused = |from, to| TraceAnswer::build(&t, from, to, NOW).unwrap_err();
    assert_eq!(
        refused(Some("<script>"), None),
        "from_utc_ms must be an integer (UTC ms)"
    );
    assert_eq!(
        refused(None, Some("1.5")),
        "to_utc_ms must be an integer (UTC ms)"
    );
    assert_eq!(
        refused(Some(""), None),
        "from_utc_ms must be an integer (UTC ms)"
    );
    assert_eq!(
        refused(Some("9"), Some("8")),
        "from_utc_ms is after to_utc_ms"
    );
    let empty = TraceAnswer::build(&t, None, None, NOW).unwrap();
    assert!(empty.rows.is_empty());
    assert_eq!(
        (empty.held, empty.oldest_utc_ms, empty.newest_utc_ms),
        (0, None, None)
    );
}

// ---- the minute summary ----

/// A clean minute writes nothing; a minute with a late boundary, or with a
/// close pair, writes one line with its counts; a pair across the minute's
/// edge counts in the minute of its second boundary; no record is summed
/// twice.
#[test]
fn a_minute_is_logged_only_when_it_held_a_clump() {
    let (t, mut w) = trace(64);
    let mut log = MinuteLog::default();
    assert_eq!(log.minute(&t), None, "nothing written yet");
    for k in 0..3 {
        on_time(&mut w, k, Some(A), SRC);
    }
    assert_eq!(log.minute(&t), None, "a clean minute");
    assert_eq!(log.minute(&t), None, "nothing new");

    on_time(&mut w, 3, Some(A), SRC);
    w.record(&marks(b(4), [40_000, 0, 0, 41_000]), Some(A), SRC, 0);
    assert_eq!(
        log.minute(&t),
        Some(MinuteSummary {
            counts: ClumpCounts {
                boundaries: 2,
                late: 1,
                close: 0,
                songs: 0,
            },
            first_utc_ms: 1_759_882_400_104,
            last_utc_ms: 1_759_882_400_174,
        })
    );
    w.record(&marks(b(5), [2_000, 0, 0, 20_000]), Some(A), SRC, 0);
    assert_eq!(log.minute(&t), None, "the late boundary is not read again");

    t.mark_song(A, 5);
    w.record(&marks(b(6), [1_000, 0, 0, 30_000]), Some(A), SRC, 0);
    assert_eq!(log.minute(&t), None, "a song alone is no clump");
    w.record(&marks(b(7), [1_000, 0, 0, 1_000]), Some(A), SRC, 0);
    w.record(&marks(b(8), [1_000, 0, 0, 4_000]), Some(A), SRC, 0);
    assert_eq!(
        log.minute(&t),
        Some(MinuteSummary {
            counts: ClumpCounts {
                boundaries: 2,
                late: 0,
                close: 1,
                songs: 0,
            },
            first_utc_ms: 1_759_882_400_234,
            last_utc_ms: 1_759_882_400_270,
        }),
        "submitted 4.33 ms after the minute before's last boundary"
    );
}

/// A minute's counts take every record of it: a song start next to a late
/// boundary is counted.
#[test]
fn a_minute_counts_its_song_starts() {
    let (t, mut w) = trace(8);
    let mut log = MinuteLog::default();
    t.mark_song(A, 3);
    on_time(&mut w, 0, Some(A), SRC);
    w.record(&marks(b(1), [50_000, 0, 0, 52_000]), Some(A), SRC, 0);
    let summary = log.minute(&t).expect("a late boundary");
    assert_eq!(
        summary.counts,
        ClumpCounts {
            boundaries: 2,
            late: 1,
            close: 0,
            songs: 1,
        }
    );
}
