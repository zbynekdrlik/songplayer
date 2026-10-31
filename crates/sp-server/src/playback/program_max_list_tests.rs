//! #243 follow-up: Spout's sender list kept by another program — when it is
//! named, when the sentinel starts (one at a time), what `max.list_holder`
//! shows, and the `program-max` thread's part (a read per boundary, the
//! release, the quiesce, the loop). Over a fake list ([`FakeList`],
//! `pub(crate)`: `api/program_tests_max.rs` reads the telemetry through the
//! router).
//! Wired via `#[cfg(test)] #[path = "program_max_list_tests.rs"] pub(crate) mod tests;`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use sp_gpu::{GpuError, LockHolder, SentinelEnd};

use super::{LIST_HELD_AFTER, ListHolderStatus, ListSide, ListWatch, Named, SpoutList};
use crate::playback::program_max::MaxOut;
use crate::playback::program_max_worker::tests::{FakeGpu, picture, plain, wait_until};
use crate::playback::program_max_worker::{MAX_RETRY_BACKOFF, MaxWorker, run_max_loop};

/// This process, as the pure watch is told it.
const OWN_PID: u32 = 8492;

/// One sender backoff, written out: the tests never read the constant they
/// pin.
const THREE_S: Duration = Duration::from_secs(3);

const MS: Duration = Duration::from_millis(1);

fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

/// Arena as the holder, pid `pid`.
fn arena(pid: u32) -> LockHolder {
    LockHolder {
        pid,
        image: Some("Arena.exe".to_string()),
    }
}

/// A pid that is not this process's (the worker watches with its own).
fn other_pid() -> u32 {
    std::process::id() ^ 1
}

/// What the fake list was asked, and what it answers.
#[derive(Default)]
pub(crate) struct FakeListState {
    /// What every read answers.
    pub holder: Option<LockHolder>,
    /// The next sentinel starts' errors (an empty queue: it starts).
    pub start_errors: VecDeque<GpuError>,
    /// What the next `sentinel_ended` answers (taken once).
    pub ended: Option<SentinelEnd>,
    pub reads: u32,
    pub starts: u32,
    pub stops: u32,
}

/// A Spout list whose holder the test sets; it counts the reads, the
/// sentinel's starts and its stops. Clones share one state.
#[derive(Clone, Default)]
pub(crate) struct FakeList(Arc<Mutex<FakeListState>>);

impl FakeList {
    pub(crate) fn state(&self) -> MutexGuard<'_, FakeListState> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(crate) fn held_by(&self, holder: Option<LockHolder>) {
        self.state().holder = holder;
    }
}

impl SpoutList for FakeList {
    fn holder(&mut self) -> Option<LockHolder> {
        let mut state = self.state();
        state.reads += 1;
        state.holder.clone()
    }

    fn start_sentinel(&mut self) -> Result<(), GpuError> {
        let mut state = self.state();
        state.starts += 1;
        match state.start_errors.pop_front() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn sentinel_ended(&mut self) -> Option<SentinelEnd> {
        self.state().ended.take()
    }

    fn stop_sentinel(&mut self) {
        self.state().stops += 1;
    }
}

#[test]
fn the_threshold_is_one_sender_backoff() {
    assert_eq!(MAX_RETRY_BACKOFF, THREE_S);
    assert_eq!(LIST_HELD_AFTER, MAX_RETRY_BACKOFF);
}

/// The same program on every read for 3 s is named once, with the
/// sentinel; `max.list_holder` counts from the first read.
#[test]
fn a_program_that_keeps_the_list_is_named_once_after_a_sender_backoff() {
    let mut watch = ListWatch::new(OWN_PID);
    let t0 = Instant::now();
    assert_eq!(watch.observe(t0, Some(arena(15128))), None);
    assert_eq!(
        watch.observe(t0 + THREE_S - MS, Some(arena(15128))),
        None,
        "not yet"
    );
    assert_eq!(watch.shown(t0 + THREE_S - MS), None, "nobody named yet");
    assert_eq!(
        watch.observe(t0 + THREE_S, Some(arena(15128))),
        Some(Named {
            holder: arena(15128),
            start_sentinel: true,
        })
    );
    assert_eq!(
        watch.shown(t0 + Duration::from_millis(4_500)),
        Some(ListHolderStatus {
            image: Some("Arena.exe".to_string()),
            pid: 15128,
            since_s: 4,
        })
    );
    assert_eq!(
        watch.observe(t0 + secs(5), Some(arena(15128))),
        None,
        "named once while it keeps it"
    );
    assert_eq!(watch.shown(t0 + secs(5)).map(|s| s.since_s), Some(5));
}

/// A free read starts the count again; so does another program.
#[test]
fn a_free_read_or_another_program_starts_the_count_again() {
    let mut watch = ListWatch::new(OWN_PID);
    let t0 = Instant::now();
    watch.observe(t0, Some(arena(15128)));
    assert_eq!(watch.observe(t0 + secs(1), None), None, "free");
    assert_eq!(watch.observe(t0 + secs(2), Some(arena(15128))), None);
    assert_eq!(
        watch.observe(t0 + secs(3), Some(arena(15128))),
        None,
        "1 s since the free read"
    );
    assert_eq!(watch.observe(t0 + secs(5) - MS, Some(arena(15128))), None);
    assert!(
        watch.observe(t0 + secs(5), Some(arena(15128))).is_some(),
        "3 s since the free read"
    );
    assert_eq!(
        watch.observe(t0 + secs(6), Some(arena(20480))),
        None,
        "another program: a new count"
    );
    assert_eq!(watch.shown(t0 + secs(6)), None, "not named yet");
    assert_eq!(
        watch.observe(t0 + secs(9), Some(arena(20480))),
        Some(Named {
            holder: arena(20480),
            start_sentinel: false,
        }),
        "named; the sentinel started for the first still waits"
    );
}

/// A thread of this process holding the list is no other program: never
/// named, however long, and it ends another program's count.
#[test]
fn this_process_holding_the_list_is_no_other_program() {
    let own = LockHolder {
        pid: OWN_PID,
        image: Some("SongPlayer.exe".to_string()),
    };
    let mut watch = ListWatch::new(OWN_PID);
    let t0 = Instant::now();
    watch.observe(t0, Some(arena(15128)));
    assert_eq!(watch.observe(t0 + secs(1), Some(own.clone())), None);
    assert_eq!(watch.observe(t0 + secs(10), Some(own)), None);
    assert_eq!(watch.shown(t0 + secs(10)), None);
    assert_eq!(watch.observe(t0 + secs(11), Some(arena(15128))), None);
    assert!(
        watch.observe(t0 + secs(14), Some(arena(15128))).is_some(),
        "3 s after this process let go"
    );
}

/// One sentinel at a time: a new one starts only once the last one ended.
#[test]
fn a_new_sentinel_starts_only_once_the_last_one_ended() {
    let mut watch = ListWatch::new(OWN_PID);
    let t0 = Instant::now();
    let start = |watch: &mut ListWatch, at: Duration| {
        watch.observe(t0 + at, Some(arena(15128)));
        watch
            .observe(t0 + at + THREE_S, Some(arena(15128)))
            .map(|named| named.start_sentinel)
    };
    assert_eq!(start(&mut watch, secs(0)), Some(true));
    watch.observe(t0 + secs(4), None);
    assert_eq!(start(&mut watch, secs(5)), Some(false), "one at a time");
    watch.sentinel_ended();
    watch.observe(t0 + secs(9), None);
    assert_eq!(start(&mut watch, secs(10)), Some(true), "after it ended");
}

/// Forgetting (MAX off) shows nobody and starts the count again; a waiting
/// sentinel stays counted.
#[test]
fn forgetting_the_holder_shows_nobody_and_starts_the_count_again() {
    let mut watch = ListWatch::new(OWN_PID);
    let t0 = Instant::now();
    watch.observe(t0, Some(arena(15128)));
    assert!(watch.observe(t0 + THREE_S, Some(arena(15128))).is_some());
    watch.forget();
    assert_eq!(watch.shown(t0 + secs(4)), None);
    assert_eq!(
        watch.observe(t0 + secs(4), Some(arena(15128))),
        None,
        "a new count"
    );
    assert_eq!(
        watch
            .observe(t0 + secs(7), Some(arena(15128)))
            .map(|named| named.start_sentinel),
        Some(false),
        "the sentinel still waits"
    );
}

/// The thread's side: a read per boundary, the sentinel started with the
/// naming, `max.list_holder`, the sentinel's end, and a new one later.
#[test]
fn the_thread_names_the_holder_starts_the_sentinel_and_reports_it() {
    let out = MaxOut::new();
    let list = FakeList::default();
    let mut side = ListSide::new(Some(Box::new(list.clone())), OWN_PID);
    let t0 = Instant::now();
    list.held_by(Some(arena(15128)));
    side.boundary(t0, &out);
    side.boundary(t0 + THREE_S - MS, &out);
    assert_eq!(out.status().list_holder, None, "not yet");
    assert_eq!(list.state().starts, 0);
    side.boundary(t0 + THREE_S, &out);
    assert_eq!(
        out.status().list_holder,
        Some(ListHolderStatus {
            image: Some("Arena.exe".to_string()),
            pid: 15128,
            since_s: 3,
        })
    );
    assert_eq!(list.state().starts, 1, "the sentinel starts with it");
    side.boundary(t0 + secs(4), &out);
    assert_eq!(list.state().starts, 1, "one sentinel");
    assert_eq!(out.status().list_holder.map(|h| h.since_s), Some(4));

    // The holder exits: the sentinel was handed the list and gave it back.
    list.state().ended = Some(SentinelEnd::GivenBack { abandoned: true });
    list.held_by(None);
    side.boundary(t0 + secs(5), &out);
    assert_eq!(out.status().list_holder, None, "nobody keeps the list");
    assert_eq!(list.state().reads, 5, "one read per boundary");

    list.held_by(Some(arena(20480)));
    side.boundary(t0 + secs(6), &out);
    side.boundary(t0 + secs(9), &out);
    assert_eq!(list.state().starts, 2, "the last one ended: a new one");
}

/// A sentinel that did not start is started again at the next holding.
#[test]
fn a_sentinel_that_did_not_start_is_started_again_at_the_next_holding() {
    let out = MaxOut::new();
    let list = FakeList::default();
    list.state()
        .start_errors
        .push_back(GpuError::Thread("no thread".to_string()));
    let mut side = ListSide::new(Some(Box::new(list.clone())), OWN_PID);
    let t0 = Instant::now();
    list.held_by(Some(arena(15128)));
    side.boundary(t0, &out);
    side.boundary(t0 + THREE_S, &out);
    assert_eq!(list.state().starts, 1);
    assert!(out.status().list_holder.is_some(), "named all the same");
    list.held_by(None);
    side.boundary(t0 + secs(4), &out);
    list.held_by(Some(arena(15128)));
    side.boundary(t0 + secs(5), &out);
    side.boundary(t0 + secs(8), &out);
    assert_eq!(list.state().starts, 2, "none waits: it starts again");
}

/// Forgetting (MAX off) leaves the sentinel waiting; stopping (the
/// quiesce) stops it.
#[test]
fn forgetting_leaves_the_sentinel_and_stopping_stops_it() {
    let out = MaxOut::new();
    let list = FakeList::default();
    let mut side = ListSide::new(Some(Box::new(list.clone())), OWN_PID);
    let t0 = Instant::now();
    list.held_by(Some(arena(15128)));
    side.boundary(t0, &out);
    side.boundary(t0 + THREE_S, &out);
    assert!(out.status().list_holder.is_some());
    side.forget(&out);
    assert_eq!(out.status().list_holder, None);
    assert_eq!(list.state().stops, 0, "it keeps waiting");
    side.boundary(t0 + secs(4), &out);
    assert_eq!(out.status().list_holder, None, "a new count");
    side.stop(&out);
    assert_eq!(list.state().stops, 1);
    assert_eq!(out.status().list_holder, None);
}

/// The worker reads the list after every boundary, forgets it when it
/// releases the GPU (MAX off), and stops the sentinel when it quiesces.
#[test]
fn the_worker_reads_the_list_after_every_boundary() {
    let max = MaxOut::new();
    max.set_enabled(true);
    let list = FakeList::default();
    list.held_by(Some(arena(other_pid())));
    let mut worker = MaxWorker::new(&max, FakeGpu::default()).with_list(Box::new(list.clone()));
    let t0 = Instant::now();
    let a = picture(4, 2);
    worker.serve(&plain(1, &a), t0);
    worker.serve(&plain(2, &a), t0 + THREE_S);
    assert_eq!(list.state().reads, 2, "one read per boundary");
    assert_eq!(list.state().starts, 1);
    assert_eq!(
        max.status().list_holder.map(|h| (h.pid, h.since_s)),
        Some((other_pid(), 3))
    );
    worker.release(Instant::now());
    assert_eq!(max.status().list_holder, None, "released: nothing shown");
    assert_eq!(list.state().stops, 0, "a release leaves the sentinel");
    worker.quiesce();
    assert_eq!(list.state().stops, 1, "a quiesce stops it");
}

/// The loop watches the list it is given, and its quiesce stops the
/// sentinel.
#[test]
fn the_loop_watches_the_list_it_is_given() {
    let max = Arc::new(MaxOut::new());
    max.set_enabled(true);
    let list = FakeList::default();
    let thread = {
        let (max, list) = (max.clone(), list.clone());
        std::thread::Builder::new()
            .name("program-max-test".into())
            .spawn(move || run_max_loop(&max, FakeGpu::default(), None, Some(Box::new(list))))
            .expect("spawn the loop")
    };
    wait_until("the loop takes jobs", || max.accepting());
    max.offer_with(|| plain(1, &picture(4, 2)));
    wait_until("the list read after the boundary", || {
        list.state().reads == 1
    });
    max.quiesce();
    // `record_quiesced` comes after the sentinel's stop.
    wait_until("quiesced", || max.status().state == "quiesced");
    assert_eq!(list.state().stops, 1, "the quiesce stopped the sentinel");
    max.stop();
    wait_until("the loop exits on stop", || thread.is_finished());
    thread.join().expect("the loop");
}

/// Off Windows Spout's list is neither read nor waited on.
#[cfg(not(windows))]
#[test]
fn off_windows_the_production_list_is_neither_read_nor_waited_on() {
    use super::SpoutListLock;
    let mut list = SpoutListLock::default();
    assert_eq!(list.holder(), None);
    assert_eq!(list.start_sentinel(), Err(GpuError::Unsupported));
    assert_eq!(list.sentinel_ended(), None);
    list.stop_sentinel();
}
