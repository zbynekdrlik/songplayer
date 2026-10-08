//! #147: the `SP-program` sender writes every boundary it serves into the
//! program trace, after its NDI submit returned: the five instants off its
//! wall, the source the bus queued it with, what it was; and the real sender
//! loop hands it the bus's source.
//! Wired via `#[cfg(test)] #[path = "program_output_tests_trace.rs"] mod tests_trace;`.

use std::cell::Cell;
use std::sync::Arc;
use std::time::Duration;

use sp_core::genlock::{GENLOCK_GRID_FPS, grid_boundary_100ns};
use sp_ndi::test_util::MockNdiBackend;
use sp_ndi::{AudioFrame, NdiSender};

use super::{CHECK_AFTER_BOUNDARY_100NS, ProgramOutput, run_program_loop};
use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_bus::{PROGRAM_NDI_NAME, ProgramBus, ProgramJob};
use crate::playback::program_trace::{ProgramTrace, TraceKind, TraceRecord};
use crate::playback::program_transition::MixJob;
use crate::playback::submit_handoff::SubmitJob;
use crate::playback::wallclock::WallClock;

const A: i64 = 11;
const B: i64 = 22;

/// The `k`-th grid boundary after 2025-10-08 00:13:20 UTC.
fn b(k: i64) -> i64 {
    grid_boundary_100ns(1_759_882_400 * GENLOCK_GRID_FPS + k, GENLOCK_GRID_FPS)
}

fn output() -> ProgramOutput<MockNdiBackend> {
    let backend = Arc::new(MockNdiBackend::new());
    let sender =
        NdiSender::new_with_clocking(backend, PROGRAM_NDI_NAME, false, false).expect("mock sender");
    ProgramOutput::new(sender, 2, 2)
}

/// A 2×2 pair (the canvas, passed through) stamped on `stamp`.
fn pair(stamp: i64, live: bool) -> SubmitJob {
    SubmitJob {
        width: 2,
        height: 2,
        stride: 2,
        video: SharedFrame::new(vec![16; 6]),
        audio: vec![AudioFrame {
            data: vec![0.0; 1600 * 2],
            channels: 2,
            sample_rate: 48_000,
            timecode_100ns: None,
        }],
        video_tc_100ns: stamp,
        audio_tc_100ns: stamp,
        live,
    }
}

/// The record a served boundary should leave under K = 0: its instants
/// `us` µs after it, its submit return's UTC.
fn expected(index: u64, stamp: i64, us: [i64; 4], source: Option<i64>) -> TraceRecord {
    TraceRecord {
        index,
        stamp_100ns: stamp,
        wire_100ns: stamp,
        utc_ms: (stamp + us[3] * 10) / 10_000,
        taken_us: us[0],
        fed_us: us[1],
        submit_start_us: us[2],
        submitted_us: us[3],
        source,
        kind: TraceKind::Source,
        live: true,
        song: None,
    }
}

/// A forward, a fill and a fade: each read off a wall that moves 1 ms per
/// read, each with the source it was served with, the song mark on the
/// first live boundary of its source.
#[test]
fn every_served_boundary_is_written_into_the_trace_after_its_submit() {
    let trace = Arc::new(ProgramTrace::with_capacity(8));
    let mut out = output().with_trace(&trace);
    let wall = Cell::new(0);
    let now = || {
        let t = wall.get();
        wall.set(t + 10_000);
        t
    };
    trace.mark_song(A, 9);

    wall.set(b(0) + 5_000);
    out.serve(ProgramJob::Source(pair(b(0), true)), Some(A), now);
    wall.set(b(1) + 2_000);
    out.serve(ProgramJob::Standby { stamp_100ns: b(1) }, Some(A), now);
    wall.set(b(2) + 1_000);
    let mix = MixJob {
        stamp_100ns: b(2),
        from: Some(pair(b(2), true)),
        to: Some(pair(b(2), true)),
        slot: 4,
        n_slots: 9,
    };
    out.serve(ProgramJob::Mix(mix), Some(B), now);

    assert_eq!(trace.written(), 3, "one record per boundary");
    assert_eq!(
        trace.records_from(0),
        [
            TraceRecord {
                kind: TraceKind::Cut,
                song: Some(9),
                ..expected(0, b(0), [500, 1_500, 2_500, 3_500], Some(A))
            },
            TraceRecord {
                kind: TraceKind::Fill,
                live: false,
                ..expected(1, b(1), [200, 1_200, 2_200, 3_200], Some(A))
            },
            TraceRecord {
                kind: TraceKind::Fade,
                ..expected(2, b(2), [100, 1_100, 2_100, 3_100], Some(B))
            },
        ]
    );
}

/// With another writer alive the output writes no trace (and still serves).
#[test]
fn an_output_whose_trace_has_another_writer_writes_nothing() {
    let trace = Arc::new(ProgramTrace::with_capacity(4));
    let _held = trace.writer().expect("the first writer");
    let mut out = output().with_trace(&trace);
    let marks = out.serve(ProgramJob::Standby { stamp_100ns: b(0) }, Some(A), || b(0));
    assert_eq!(marks.stamp_100ns, b(0));
    assert_eq!(trace.written(), 0);
}

/// The real sender loop: a selected source that sends nothing has its
/// boundary filled, and the trace names it as that boundary's source.
#[test]
fn the_sender_loop_traces_the_source_the_bus_queued_a_boundary_with() {
    let bus = Arc::new(ProgramBus::new());
    bus.select_initial(A, None);
    let out = output().with_trace(bus.trace());
    let (wall, _clock) = WallClock::settable(b(0) + CHECK_AFTER_BOUNDARY_100NS);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let thread = {
        let bus = bus.clone();
        std::thread::spawn(move || {
            let (mut out, mut wall) = (out, wall);
            run_program_loop(&mut out, &bus, &mut wall);
            let _ = done_tx.send(());
            out
        })
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while bus.status().health.submitted < 1 && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    bus.stop();
    done_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("the sender thread exits once the bus is stopped");
    let _out = thread.join().unwrap();
    let record = bus.trace().read(0).expect("the filled boundary");
    assert_eq!(
        (record.stamp_100ns, record.kind, record.source),
        (b(0), TraceKind::Fill, Some(A))
    );
}
