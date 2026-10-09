//! #223 S2: the `program-max` thread's log ([`LogGate`]): each change once
//! per window and never lost, the worker's lasting failure held back inside
//! the window, and the log's time. Moved out of `program_max_worker_tests.rs`
//! for the 1000-line cap (#239).
//! Wired via `#[cfg(test)] #[path = "program_max_worker_tests_log.rs"] mod tests_log;`.

use std::time::{Duration, Instant};

use sp_gpu::GpuError;

use super::tests::{FakeGpu, device_lost};
use super::{LogGate, LogLine, MAX_LOG_EVERY_100NS, MaxWorker, elapsed_100ns};
use crate::playback::program_max::{MaxJob, MaxOut};

#[test]
fn the_log_writes_each_change_once_per_window_and_never_loses_one() {
    let s = 10_000_000; // one second, in 100 ns
    assert_eq!(MAX_LOG_EVERY_100NS, 5 * s);
    let mut gate = LogGate::default();
    assert_eq!(gate.observe(0, None), None, "going out from the start");
    assert_eq!(
        gate.observe(0, Some("X")),
        Some(LogLine::Failing { held_back: 0 })
    );
    assert_eq!(gate.observe(s, Some("X")), None, "the same failure again");
    assert_eq!(
        gate.observe(s, Some("Y")),
        None,
        "another failure inside the window: held back"
    );
    assert_eq!(gate.observe(2 * s, Some("Y")), None);
    assert_eq!(
        gate.observe(5 * s, Some("Y")),
        Some(LogLine::Failing { held_back: 2 }),
        "the held-back failure is written once the window is over"
    );
    assert_eq!(gate.observe(6 * s, None), None, "a recovery inside it");
    assert_eq!(
        gate.observe(10 * s, None),
        Some(LogLine::Recovered { held_back: 1 })
    );
    assert_eq!(gate.observe(11 * s, Some("Z")), None, "held back");
    assert_eq!(
        gate.observe(16 * s, None),
        None,
        "a held-back change that reverted is not written: the log's last line is true again"
    );
}

/// The worker's side: a change held back inside the window (the device
/// lost, then no adapter for the rebuild) is written once the window is
/// over, as the state is then, and the recovery after it.
#[test]
fn the_worker_logs_a_lasting_failure_held_back_inside_the_window() {
    let max = MaxOut::new();
    max.set_enabled(true);
    let gpu = FakeGpu::default();
    gpu.script().compose.push_back(device_lost());
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    let at = |ms: u64| t0 + Duration::from_millis(ms);
    let black = |stamp: i64| MaxJob::Black { stamp_100ns: stamp };
    assert_eq!(
        worker.serve(&black(1), at(0)),
        Some(LogLine::Failing { held_back: 0 }),
        "the lost device"
    );
    // The rebuild (and the one after the backoff) finds no adapter.
    gpu.script().compositor.push_back(GpuError::NoAdapter);
    gpu.script().compositor.push_back(GpuError::NoAdapter);
    assert_eq!(
        worker.serve(&black(2), at(33)),
        None,
        "no adapter: held back"
    );
    assert_eq!(
        worker.serve(&black(3), at(1_000)),
        None,
        "a skipped boundary"
    );
    assert_eq!(
        worker.serve(&black(4), at(5_000)),
        Some(LogLine::Failing { held_back: 2 }),
        "the lasting failure is written once the window is over"
    );
    assert_eq!(
        max.status().state,
        format!("error: {}", GpuError::NoAdapter)
    );
    assert_eq!(
        worker.serve(&black(5), at(10_000)),
        Some(LogLine::Recovered { held_back: 0 })
    );
    assert_eq!(gpu.log().sent, 1);
}

#[test]
fn the_log_time_is_the_threads_time_in_100ns() {
    let t0 = Instant::now();
    assert_eq!(elapsed_100ns(t0, t0), 0);
    assert_eq!(
        elapsed_100ns(t0, t0 + Duration::from_millis(1500)),
        15_000_000
    );
    assert_eq!(
        elapsed_100ns(t0 + Duration::from_secs(1), t0),
        0,
        "never negative"
    );
}
