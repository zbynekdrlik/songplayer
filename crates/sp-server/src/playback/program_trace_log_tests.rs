//! #147: the program trace's once-a-minute summary task
//! (`program_trace_log.rs`) on a paused clock: a minute that held a clump
//! boundary is handed to the writer once, a clean minute is not, nothing
//! before the first full minute, and shutdown ends it.
//! Wired via `#[cfg(test)] #[path = "program_trace_log_tests.rs"] mod tests;`.

use std::sync::Arc;
use std::time::Duration;

use sp_core::genlock::{GENLOCK_GRID_FPS, grid_boundary_100ns};
use tokio::sync::{broadcast, mpsc};

use super::*;
use crate::playback::program_output_timing::BoundaryMarks;
use crate::playback::program_trace::{JobShape, MinuteSummary, ProgramTrace, TraceKind};

const MINUTE: Duration = Duration::from_secs(60);

/// Boundary `k` after 2025-10-08 00:13:20 UTC, its job taken `taken_us`
/// after it and its submit returned 4 ms later.
fn boundary(k: i64, taken_us: i64) -> BoundaryMarks {
    let stamp = grid_boundary_100ns(1_759_882_400 * GENLOCK_GRID_FPS + k, GENLOCK_GRID_FPS);
    let taken = stamp + taken_us * 10;
    BoundaryMarks {
        stamp_100ns: stamp,
        taken_100ns: taken,
        fed_100ns: taken,
        submit_start_100ns: taken,
        submitted_100ns: taken + 40_000,
    }
}

const SRC: JobShape = JobShape {
    kind: TraceKind::Source,
    live: true,
};

#[tokio::test(start_paused = true)]
async fn the_task_hands_on_only_a_minute_that_held_a_clump() {
    let trace = Arc::new(ProgramTrace::with_capacity(64));
    let mut w = trace.writer().expect("the first writer");
    let (lines, mut written) = mpsc::unbounded_channel::<MinuteSummary>();
    let (stop, _) = broadcast::channel(1);
    let task = tokio::spawn(run_trace_log(
        trace.clone(),
        stop.subscribe(),
        MINUTE,
        move |summary: &MinuteSummary| lines.send(*summary).expect("the test listens"),
    ));

    // A late boundary right away: summed only once a full minute passed.
    w.record(&boundary(0, 40_000), Some(1), SRC, 0);
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert!(
        written.try_recv().is_err(),
        "no summary before the first minute"
    );
    tokio::time::sleep(Duration::from_secs(31)).await;
    let first = written
        .try_recv()
        .expect("the first minute held a late boundary");
    assert_eq!((first.counts.boundaries, first.counts.late), (1, 1));

    // A clean minute is handed on to nobody.
    w.record(&boundary(3, 1_000), Some(1), SRC, 0);
    tokio::time::sleep(MINUTE).await;
    assert!(written.try_recv().is_err(), "a clean minute");

    // The next one with a late boundary is.
    w.record(&boundary(6, 50_000), Some(1), SRC, 0);
    tokio::time::sleep(MINUTE).await;
    let third = written.try_recv().expect("a late boundary again");
    assert_eq!((third.counts.boundaries, third.counts.late), (1, 1));

    stop.send(()).expect("the task listens");
    tokio::time::timeout(Duration::from_secs(600), task)
        .await
        .expect("shutdown ends the task")
        .expect("the task did not panic");
}
