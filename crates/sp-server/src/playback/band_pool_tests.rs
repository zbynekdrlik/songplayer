//! #223 follow-up (design record 5973498519): the `SP-program` sender's
//! persistent band workers. WHERE each band ran is read from the thread
//! itself (`thread::current().id()` / `.name()`, the "prove where a call
//! ran" pattern), never from a wall time: band 0 on the calling thread, band
//! `i` on the worker `<name>-<i>`, the SAME threads picture after picture,
//! and none of them left once the pool is dropped. The waits are gates the
//! test holds; a "must not have returned yet" window is used only in the
//! safe direction (correct code can never fail it). Every gate is opened
//! when a check fails (its sender or opener is dropped as the test unwinds),
//! and the gated tests declare their pool after what its painter borrows, so
//! its drop joins the workers before those are freed.
//! Wired via `#[cfg(test)] #[path = "band_pool_tests.rs"] mod tests;`.

use std::any::Any;
use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc;
use std::sync::{Condvar, Mutex};
use std::thread::{self, ThreadId};
use std::time::Duration;

use super::{BandPool, panic_text};

/// Where one band was painted: the band, its thread, the thread's name.
type Painted = (usize, ThreadId, Option<String>);

/// A painter that records where each band ran.
fn recorder(log: &Mutex<Vec<Painted>>) -> impl Fn(usize) + Sync + '_ {
    move |band| {
        let me = thread::current();
        let name = me.name().map(str::to_owned);
        log.lock().unwrap().push((band, me.id(), name));
    }
}

/// The pool whose workers [`exit_hold`] holds at their end: only the drop
/// test's (every other pool's workers end at once).
const HELD: &str = "exit-held";

/// Whether the held workers may end (the drop test opens it).
static EXIT_OPEN: Mutex<bool> = Mutex::new(false);
static EXIT_OPENED: Condvar = Condvar::new();

/// The last step of a worker (`work`, test builds only): a worker of the
/// [`HELD`] pool waits until the drop test opens the gate.
pub(super) fn exit_hold() {
    if thread::current()
        .name()
        .is_some_and(|name| name.starts_with(HELD))
    {
        let mut open = EXIT_OPEN.lock().unwrap();
        while !*open {
            open = EXIT_OPENED.wait(open).unwrap();
        }
    }
}

/// Opens the exit gate when dropped: at the end of the drop test, or as it
/// unwinds from a failed check (so no held worker is left waiting).
struct ExitOpener;

impl Drop for ExitOpener {
    fn drop(&mut self) {
        *EXIT_OPEN.lock().unwrap_or_else(|e| e.into_inner()) = true;
        EXIT_OPENED.notify_all();
    }
}

#[test]
fn a_pool_starts_one_worker_per_band_past_the_first() {
    for (asked, bands, workers) in [(0, 1, 0), (1, 1, 0), (2, 2, 1), (6, 6, 5)] {
        let pool = BandPool::new("pool-size", asked);
        assert_eq!(
            (pool.bands(), pool.workers()),
            (bands, workers),
            "{asked} bands asked: band 0 is the caller's, one worker per further band"
        );
    }
}

#[test]
fn every_band_past_the_first_runs_on_its_own_worker_picture_after_picture() {
    // The fix itself: before, every picture started K − 1 scoped threads, a
    // new ThreadId each time. Now band i runs on the worker `<name>-<i>`,
    // the same thread on every run.
    let pool = BandPool::new("band-test", 4);
    let caller = thread::current().id();
    let mut workers: Option<Vec<ThreadId>> = None;
    for picture in 0..5 {
        let log = Mutex::new(Vec::new());
        assert_eq!(
            pool.run(&recorder(&log)),
            4,
            "picture {picture}: four threads painted"
        );
        let mut painted = log.into_inner().unwrap();
        painted.sort_by_key(|&(band, ..)| band);
        assert_eq!(
            painted.iter().map(|&(band, ..)| band).collect::<Vec<_>>(),
            vec![0, 1, 2, 3],
            "picture {picture}: every band painted exactly once"
        );
        assert_eq!(
            painted[0].1, caller,
            "picture {picture}: band 0 on the calling thread"
        );
        for (band, _, name) in &painted[1..] {
            assert_eq!(
                name.as_deref(),
                Some(format!("band-test-{band}").as_str()),
                "picture {picture}: band {band} on its own worker"
            );
        }
        let ids: Vec<ThreadId> = painted[1..].iter().map(|&(_, id, _)| id).collect();
        assert!(
            ids.iter().all(|&id| id != caller),
            "picture {picture}: no worker band on the calling thread"
        );
        match &workers {
            None => workers = Some(ids),
            Some(first) => assert_eq!(
                &ids, first,
                "picture {picture}: the same worker threads as the first picture"
            ),
        }
    }
    let ids = workers.unwrap();
    assert!(
        ids[0] != ids[1] && ids[1] != ids[2] && ids[0] != ids[2],
        "a thread of its own per band: {ids:?}"
    );
}

#[test]
fn a_pool_of_one_band_paints_on_the_calling_thread_alone() {
    let pool = BandPool::new("one-band", 1);
    let log = Mutex::new(Vec::new());
    assert_eq!(pool.run(&recorder(&log)), 1, "one thread painted");
    let painted = log.into_inner().unwrap();
    assert_eq!(painted.len(), 1);
    assert_eq!(
        (painted[0].0, painted[0].1),
        (0, thread::current().id()),
        "band 0, on the calling thread"
    );
}

#[test]
fn a_run_returns_only_once_every_band_is_painted() {
    // Band 1 is held on a gate the test opens; the run must not return
    // before it (the window below can only pass vacuously on a slow runner,
    // never fail correct code), and once it returns band 1 is done.
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let events = Mutex::new(Vec::new());
    let paint = |band: usize| {
        if band == 1 {
            started_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
            events.lock().unwrap().push("band 1 painted");
        }
    };
    let (returned_tx, returned_rx) = mpsc::channel();
    let pool = BandPool::new("wait-test", 2);
    thread::scope(|s| {
        // Owned here: a failed check below drops it as it unwinds, so the
        // held band's `recv` fails instead of hanging the scope's join.
        let release_tx = release_tx;
        s.spawn(|| {
            let threads = pool.run(&paint);
            events.lock().unwrap().push("run returned");
            returned_tx.send(threads).unwrap();
        });
        started_rx
            .recv_timeout(Duration::from_secs(20))
            .expect("band 1 started on its worker");
        assert!(
            returned_rx
                .recv_timeout(Duration::from_millis(200))
                .is_err(),
            "the run is still waiting for band 1"
        );
        release_tx.send(()).unwrap();
        assert_eq!(
            returned_rx.recv_timeout(Duration::from_secs(20)),
            Ok(2),
            "the run returned once band 1 was painted: two threads"
        );
    });
    drop(pool);
    assert_eq!(
        events.into_inner().unwrap(),
        vec!["band 1 painted", "run returned"]
    );
}

#[test]
fn a_band_that_panics_on_a_worker_panics_the_run_and_the_worker_lives_on() {
    // As with `std::thread::scope`, the caller panics once every band is
    // done (naming the band's message, so the panic hook records the caller
    // too). The worker caught it, so the next picture runs on the same
    // threads.
    let pool = BandPool::new("panic-test", 3);
    let log = Mutex::new(Vec::new());
    {
        let record = recorder(&log);
        let paint = |band: usize| {
            record(band);
            if band == 1 {
                panic!("band 1 broke");
            }
        };
        let caught = panic::catch_unwind(AssertUnwindSafe(|| pool.run(&paint)));
        let payload = caught.expect_err("the band's panic reaches the caller");
        assert_eq!(
            payload.downcast_ref::<String>().map(String::as_str),
            Some("band pool: a band panicked on its worker: band 1 broke"),
            "a panic of the caller's own, naming the band's"
        );
    }
    let mut first = log.into_inner().unwrap();
    first.sort_by_key(|&(band, ..)| band);
    assert_eq!(
        first.iter().map(|&(band, ..)| band).collect::<Vec<_>>(),
        vec![0, 1, 2],
        "every band ran before the panic reached the caller"
    );

    let log = Mutex::new(Vec::new());
    assert_eq!(
        pool.run(&recorder(&log)),
        3,
        "the next picture: three threads"
    );
    let mut next = log.into_inner().unwrap();
    next.sort_by_key(|&(band, ..)| band);
    assert_eq!(
        next.iter().map(|&(_, id, _)| id).collect::<Vec<_>>(),
        first.iter().map(|&(_, id, _)| id).collect::<Vec<_>>(),
        "the same threads, the panicked band's worker included"
    );
}

#[test]
fn the_callers_panic_names_the_bands_message_whatever_its_payload() {
    // `panic!("literal")` makes a `&str`, `panic!("{x}")` a `String`; any
    // other payload (`panic_any`) has no text to name.
    let text: Box<dyn Any + Send> = Box::new("band 2 broke");
    let owned: Box<dyn Any + Send> = Box::new(format!("band {} broke", 3));
    let number: Box<dyn Any + Send> = Box::new(42_u32);
    assert_eq!(panic_text(&*text), "band 2 broke");
    assert_eq!(panic_text(&*owned), "band 3 broke");
    assert_eq!(panic_text(&*number), "a payload that is not text");
}

#[test]
fn a_panic_on_the_calling_thread_waits_for_the_workers_bands() {
    // Band 0 (the caller's) panics while band 1 still paints on its worker.
    // The unwinding run must not leave before band 1 is done: band 1 still
    // uses the caller's painter and buffers.
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let events = Mutex::new(Vec::new());
    let paint = |band: usize| {
        if band == 1 {
            started_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
            events.lock().unwrap().push("band 1 painted");
        } else {
            panic!("band 0 broke");
        }
    };
    let (returned_tx, returned_rx) = mpsc::channel();
    let pool = BandPool::new("unwind-test", 2);
    thread::scope(|s| {
        // Owned here: a failed check below drops it as it unwinds, so the
        // held band's `recv` fails instead of hanging the scope's join.
        let release_tx = release_tx;
        s.spawn(|| {
            let caught = panic::catch_unwind(AssertUnwindSafe(|| pool.run(&paint)));
            events.lock().unwrap().push("run unwound");
            returned_tx.send(caught.is_err()).unwrap();
        });
        started_rx
            .recv_timeout(Duration::from_secs(20))
            .expect("band 1 started on its worker");
        assert!(
            returned_rx
                .recv_timeout(Duration::from_millis(200))
                .is_err(),
            "the unwinding run is still waiting for band 1"
        );
        release_tx.send(()).unwrap();
        assert_eq!(
            returned_rx.recv_timeout(Duration::from_secs(20)),
            Ok(true),
            "band 0's panic reached the caller"
        );
    });
    drop(pool);
    assert_eq!(
        events.into_inner().unwrap(),
        vec!["band 1 painted", "run unwound"]
    );
}

#[test]
fn the_pool_shuts_its_workers_down_when_it_is_dropped() {
    // Dropping the pool closes the queues and JOINS the workers: with the
    // workers held at their last step (`exit_hold`), the drop must still be
    // waiting (a safe-direction window), and once they may end it returns
    // with none of them left — every worker holds the pool's token to its
    // last step.
    let log = Mutex::new(Vec::new());
    let pool = BandPool::new(HELD, 6);
    // Declared after the pool: a failed check below drops it first, so the
    // held workers may end before the pool's drop joins them.
    let opener = ExitOpener;
    let alive = pool.alive();
    assert_eq!(alive.strong_count(), 5, "five workers running");
    assert_eq!(pool.run(&recorder(&log)), 6);
    let (dropped_tx, dropped_rx) = mpsc::channel();
    thread::scope(|s| {
        s.spawn(move || {
            drop(pool);
            dropped_tx.send(()).unwrap();
        });
        assert!(
            dropped_rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "the drop is waiting for its held workers"
        );
        assert_eq!(alive.strong_count(), 5, "the held workers are all there");
        drop(opener);
        dropped_rx
            .recv_timeout(Duration::from_secs(20))
            .expect("the drop returned once its workers could end");
        assert_eq!(
            alive.strong_count(),
            0,
            "every worker thread ended before the drop returned"
        );
    });

    let single = BandPool::new("drop-test-one", 1);
    let alive = single.alive();
    assert_eq!(alive.strong_count(), 0, "one band: no worker at all");
    drop(single);
    assert_eq!(alive.strong_count(), 0);
}
