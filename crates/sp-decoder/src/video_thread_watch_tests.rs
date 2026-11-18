//! #319: the watcher of a dropped reader's thread — the verdict of its wait,
//! nothing counted for a thread that ended, one stuck reader counted (and
//! `stuck_now` given back when its read returns) for one that did not. Every
//! wait is bounded; the only short wait is the watcher's own, and a thread
//! whose sender is kept can only ever read as stuck.

use std::time::{Duration, Instant};

use crossbeam_channel::RecvTimeoutError;

use super::{Detached, DetachedCounts, DetachedReaders, detached_readers, detached_verdict, watch};

/// A bound no correct step comes near.
const BOUND: Duration = Duration::from_secs(20);

/// How long a count may take to reach its value: far above a 20-30 ms
/// watch plus a thread start, and under cargo-mutants' 20 s test timeout,
/// so a mutant that never counts FAILS the test instead of hanging it
/// (a TIMEOUT fails the mutation gate like a survivor).
const COUNT_BOUND: Duration = Duration::from_secs(5);

/// Wait (bounded) until `counts` reads `want`.
fn wait_for(counts: &DetachedReaders, want: DetachedCounts) {
    let start = Instant::now();
    while counts.snapshot() != want {
        assert!(
            start.elapsed() < COUNT_BOUND,
            "counts {:?}, want {want:?}",
            counts.snapshot()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn only_a_wait_that_ran_out_is_a_stuck_thread() {
    assert_eq!(
        detached_verdict(Err(RecvTimeoutError::Timeout)),
        Detached::Stuck
    );
    assert_eq!(
        detached_verdict(Err(RecvTimeoutError::Disconnected)),
        Detached::Ended
    );
    assert_eq!(detached_verdict(Ok(())), Detached::Ended);
}

#[test]
fn a_thread_that_ended_is_not_counted() {
    let counts = DetachedReaders::new();
    let (done_tx, done) = crossbeam_channel::bounded::<()>(0);
    drop(done_tx);
    assert_eq!(watch(&done, BOUND, "t.mp4", &counts), Detached::Ended);
    assert_eq!(counts.snapshot(), DetachedCounts::default());
}

#[test]
fn a_thread_still_in_its_read_is_counted_until_the_read_returns() {
    let counts: &'static DetachedReaders = Box::leak(Box::default());
    let (done_tx, done) = crossbeam_channel::bounded::<()>(0);
    let watcher =
        std::thread::spawn(move || watch(&done, Duration::from_millis(20), "stuck.mp4", counts));
    wait_for(
        counts,
        DetachedCounts {
            stuck: 1,
            stuck_now: 1,
        },
    );
    // The read returns: the thread ends.
    drop(done_tx);
    assert_eq!(watcher.join().unwrap(), Detached::Stuck);
    assert_eq!(
        counts.snapshot(),
        DetachedCounts {
            stuck: 1,
            stuck_now: 0
        }
    );
}

#[test]
fn the_process_counts_live_in_one_place() {
    assert!(std::ptr::eq(detached_readers(), detached_readers()));
}
