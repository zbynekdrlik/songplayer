//! Per-sender locking for [`crate::RealNdiBackend`] (#147 round 11).
//!
//! The backend used to keep every sender in ONE `Mutex<HashMap<..>>` and hold
//! it across each NDI SDK call. `NDIlib_send_send_video_async_v2` blocks until
//! the SDK has finished with the previous frame of THAT sender, so with genlock
//! pacing (every output emits on the same 33.3 ms boundary) one slow sender
//! delayed the video AND audio sends of every other output.
//!
//! `HandleTable` splits that lock in two:
//! - the map sits behind an `RwLock` that is held only to insert, remove, or
//!   clone one slot's `Arc` — never across an operation;
//! - each slot has its own `Mutex`, held for the whole operation, so the calls
//!   on one sender stay ordered while different senders run in parallel;
//! - a slot is an `Option<T>`: [`HandleTable::remove_with`] takes the state out
//!   under the slot lock, so an operation that cloned the `Arc` before the
//!   remove finds `None` afterwards and does nothing (no use-after-destroy).
//!
//! Generic over `T` so the locking is unit-tested with plain values and
//! threads, without the NDI SDK.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

/// One handle's state behind its own lock. `None` once the handle is removed.
type Slot<T> = Arc<Mutex<Option<T>>>;

const POISONED: &str = "sp-ndi handle table lock poisoned";

/// Handle id → per-handle locked state.
pub(crate) struct HandleTable<T> {
    map: RwLock<HashMap<usize, Slot<T>>>,
}

impl<T> HandleTable<T> {
    pub(crate) fn new() -> Self {
        Self {
            map: RwLock::new(HashMap::new()),
        }
    }

    /// Register `state` under `id`.
    pub(crate) fn insert(&self, id: usize, state: T) {
        self.map
            .write()
            .expect(POISONED)
            .insert(id, Arc::new(Mutex::new(Some(state))));
    }

    /// Clone handle `id`'s slot. The map's read lock is released when this
    /// returns, before anyone locks the slot.
    fn slot(&self, id: usize) -> Option<Slot<T>> {
        self.map.read().expect(POISONED).get(&id).cloned()
    }

    /// Run `op` on handle `id`'s state while holding that handle's lock (and
    /// ONLY that lock). `None` (and `op` never runs) when the handle does not
    /// exist or was removed.
    pub(crate) fn with<R>(&self, id: usize, op: impl FnOnce(&mut T) -> R) -> Option<R> {
        let slot = self.slot(id)?;
        run_live(&slot, op)
    }

    /// Remove handle `id` and hand its state to `teardown`. Waits for an
    /// operation in flight on that handle, and runs `teardown` while holding
    /// the handle's lock. `None` (and `teardown` never runs) when the handle
    /// does not exist.
    pub(crate) fn remove_with<R>(&self, id: usize, teardown: impl FnOnce(T) -> R) -> Option<R> {
        // The map's write lock is released at the end of this statement, so
        // waiting for the in-flight op below never blocks the other handles.
        let slot = self.map.write().expect(POISONED).remove(&id)?;
        // A panicked op poisoned this handle only. Tear it down anyway, so the
        // SDK sender is still destroyed rather than leaked.
        let mut guard = slot.lock().unwrap_or_else(PoisonError::into_inner);
        let state = guard.take()?;
        let out = teardown(state);
        drop(guard);
        Some(out)
    }
}

/// Lock one slot and run `op` on its state, or return `None` if the handle was
/// removed in the meantime.
fn run_live<T, R>(slot: &Mutex<Option<T>>, op: impl FnOnce(&mut T) -> R) -> Option<R> {
    let mut guard = slot.lock().expect(POISONED);
    (*guard).as_mut().map(op)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::thread;
    use std::time::Duration;

    /// Upper bound for a step that MUST happen. Never reached when correct.
    const MUST: Duration = Duration::from_secs(5);
    /// Window in which a step that must NOT happen yet would have shown up.
    const MUST_NOT: Duration = Duration::from_millis(200);

    type Log = Vec<&'static str>;

    /// Start an operation on handle `id` on its own thread, and return once it
    /// is running (holding `id`'s lock). It stands in for a blocking SDK call:
    /// it returns only after the returned sender is signalled.
    fn hold(
        table: &Arc<HandleTable<Log>>,
        id: usize,
    ) -> (mpsc::Sender<()>, thread::JoinHandle<Option<()>>) {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let t = Arc::clone(table);
        let join = thread::spawn(move || {
            t.with(id, |log| {
                log.push("held-start");
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                log.push("held-end");
            })
        });
        entered_rx
            .recv_timeout(MUST)
            .expect("the held operation never started");
        (release_tx, join)
    }

    #[test]
    fn an_op_on_another_handle_completes_while_one_handle_is_blocked() {
        let table = Arc::new(HandleTable::new());
        table.insert(1, Log::new());
        table.insert(2, Log::new());
        let (release, held) = hold(&table, 1);

        let (done_tx, done_rx) = mpsc::channel();
        let t = Arc::clone(&table);
        let other = thread::spawn(move || {
            let r = t.with(2, |log| {
                log.push("other");
                7
            });
            done_tx.send(r).unwrap();
        });
        let got = done_rx.recv_timeout(MUST);

        release.send(()).unwrap();
        assert_eq!(held.join().unwrap(), Some(()));
        other.join().unwrap();
        assert_eq!(
            got,
            Ok(Some(7)),
            "handle 2 waited for handle 1's blocked op"
        );
        assert_eq!(table.with(2, |log| log.clone()), Some(vec!["other"]));
    }

    #[test]
    fn create_and_destroy_of_other_handles_proceed_while_one_handle_is_blocked() {
        let table = Arc::new(HandleTable::new());
        table.insert(1, Log::new());
        table.insert(2, vec!["two"]);
        let (release, held) = hold(&table, 1);

        let (done_tx, done_rx) = mpsc::channel();
        let t = Arc::clone(&table);
        let admin = thread::spawn(move || {
            t.insert(3, vec!["three"]);
            done_tx.send(t.remove_with(2, |log| log)).unwrap();
        });
        let got = done_rx.recv_timeout(MUST);

        release.send(()).unwrap();
        assert_eq!(held.join().unwrap(), Some(()));
        admin.join().unwrap();
        assert_eq!(
            got,
            Ok(Some(vec!["two"])),
            "create/destroy waited for handle 1"
        );
        assert_eq!(table.with(3, |log| log.clone()), Some(vec!["three"]));
        assert_eq!(table.with(2, |_| ()), None);
    }

    #[test]
    fn two_ops_on_the_same_handle_never_overlap() {
        let table = Arc::new(HandleTable::new());
        table.insert(1, Log::new());
        let (release, held) = hold(&table, 1);

        let (entered_tx, entered_rx) = mpsc::channel();
        let t = Arc::clone(&table);
        let second = thread::spawn(move || {
            t.with(1, |log| {
                log.push("second");
                entered_tx.send(()).unwrap();
            })
        });
        assert_eq!(
            entered_rx.recv_timeout(MUST_NOT),
            Err(RecvTimeoutError::Timeout),
            "a second op entered the handle while the first was still running"
        );

        release.send(()).unwrap();
        entered_rx
            .recv_timeout(MUST)
            .expect("the second op never ran after the first ended");
        assert_eq!(held.join().unwrap(), Some(()));
        assert_eq!(second.join().unwrap(), Some(()));
        assert_eq!(
            table.with(1, |log| log.clone()),
            Some(vec!["held-start", "held-end", "second"])
        );
    }

    #[test]
    fn remove_waits_for_the_in_flight_op_and_a_later_lookup_finds_nothing() {
        let table = Arc::new(HandleTable::new());
        table.insert(1, Log::new());
        let (release, held) = hold(&table, 1);

        let (destroyed_tx, destroyed_rx) = mpsc::channel();
        let t = Arc::clone(&table);
        let destroyer = thread::spawn(move || {
            let log = t.remove_with(1, |mut log| {
                log.push("destroy");
                log
            });
            destroyed_tx.send(()).unwrap();
            log
        });
        assert_eq!(
            destroyed_rx.recv_timeout(MUST_NOT),
            Err(RecvTimeoutError::Timeout),
            "destroy ran while an op on the handle was in flight"
        );

        release.send(()).unwrap();
        destroyed_rx
            .recv_timeout(MUST)
            .expect("destroy never ran after the op ended");
        assert_eq!(held.join().unwrap(), Some(()));
        assert_eq!(
            destroyer.join().unwrap(),
            Some(vec!["held-start", "held-end", "destroy"])
        );
        assert_eq!(table.with(1, |_| ()), None);
        assert_eq!(table.remove_with(1, |_| ()), None);
    }

    #[test]
    fn an_op_that_looked_up_the_handle_before_remove_does_nothing_after_it() {
        let table: HandleTable<Log> = HandleTable::new();
        table.insert(1, vec!["live"]);
        // A send that cloned the slot, then lost the race to a destroy.
        let late = table.slot(1).expect("an inserted handle has a slot");
        assert_eq!(table.remove_with(1, |log| log), Some(vec!["live"]));

        let mut ran = false;
        assert_eq!(run_live(&late, |_| ran = true), None);
        assert!(!ran, "a late op reached the destroyed state");
    }

    #[test]
    fn remove_still_tears_down_a_handle_an_op_panicked_on() {
        let table = Arc::new(HandleTable::new());
        table.insert(1, vec!["live"]);
        let t = Arc::clone(&table);
        let op = thread::spawn(move || {
            t.with(1, |log| {
                if log.len() == 1 {
                    panic!("an op panicked while holding the handle's lock");
                }
            })
        });
        assert!(op.join().is_err(), "the op must have panicked");

        assert_eq!(table.remove_with(1, |log| log), Some(vec!["live"]));
        assert_eq!(table.with(1, |_| ()), None);
    }

    #[test]
    fn an_unknown_handle_runs_nothing() {
        let table: HandleTable<Log> = HandleTable::new();
        table.insert(1, vec!["one"]);
        let mut ran = false;
        assert_eq!(table.with(2, |_| ran = true), None);
        assert_eq!(table.remove_with(2, |_| ran = true), None);
        assert!(!ran);
        assert_eq!(table.with(1, |log| log.clone()), Some(vec!["one"]));
    }
}
