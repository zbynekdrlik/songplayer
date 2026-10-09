//! #223 follow-up: the constant-phase Spout send (`program_max_send.rs`):
//! the due instant, the wait's sleep/spin decision, and `send_at` on a
//! clock that never sleeps ([`FakeClock`], also used by
//! `program_max_worker_tests_send.rs`).
//! Wired via `#[cfg(test)] #[path = "program_max_send_tests.rs"] pub(crate) mod tests;`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{
    MAX_SEND_LEAD, SEND_SPIN_MARGIN, SendClock, SendTiming, WaitStep, send_at, send_due,
    send_wait_step,
};

/// A clock that never sleeps: `now` is the test's; a wait runs `on_wait`
/// (what has happened by then), records its due instant and moves `now`
/// to it.
pub(crate) struct FakeClock {
    pub(crate) now: Instant,
    pub(crate) waits: Arc<Mutex<Vec<Instant>>>,
    pub(crate) on_wait: Option<Box<dyn FnMut()>>,
}

impl FakeClock {
    /// A clock at `now`, and the list its waits are recorded in.
    pub(crate) fn at(now: Instant) -> (Self, Arc<Mutex<Vec<Instant>>>) {
        let waits = Arc::new(Mutex::new(Vec::new()));
        let clock = Self {
            now,
            waits: waits.clone(),
            on_wait: None,
        };
        (clock, waits)
    }
}

impl SendClock for FakeClock {
    fn now(&mut self) -> Instant {
        self.now
    }

    fn wait_until(&mut self, due: Instant) {
        if let Some(hook) = self.on_wait.as_mut() {
            hook();
        }
        self.waits
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(due);
        self.now = self.now.max(due);
    }
}

pub(crate) fn waits(list: &Arc<Mutex<Vec<Instant>>>) -> Vec<Instant> {
    list.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

const MS: Duration = Duration::from_millis(1);

#[test]
fn a_boundary_is_due_twelve_ms_after_its_offer() {
    let offered = Instant::now();
    assert_eq!(MAX_SEND_LEAD, 12 * MS);
    assert_eq!(send_due(offered), offered + 12 * MS);
}

#[test]
fn the_wait_sleeps_to_the_spin_margin_then_spins_until_due() {
    assert_eq!(SEND_SPIN_MARGIN, 2 * MS);
    assert_eq!(send_wait_step(Duration::ZERO), WaitStep::Done);
    assert_eq!(send_wait_step(5 * MS), WaitStep::Sleep(3 * MS));
    assert_eq!(
        send_wait_step(2 * MS + Duration::from_micros(1)),
        WaitStep::Sleep(Duration::from_micros(1))
    );
    assert_eq!(
        send_wait_step(2 * MS),
        WaitStep::Spin,
        "at the margin: spin"
    );
    assert_eq!(send_wait_step(Duration::from_micros(1)), WaitStep::Spin);
}

/// The bug: the send went out the moment the compose was done. A compose
/// done 5 ms after the offer now waits for the due instant, then sends.
#[test]
fn an_early_compose_waits_for_the_due_instant_then_sends() {
    let offered = Instant::now();
    let (mut clock, list) = FakeClock::at(offered + 5 * MS);
    let mut waits_at_send = None;
    let (sent, timing) = send_at(&mut clock, send_due(offered), offered, || {
        waits_at_send = Some(waits(&list).len());
        Ok::<_, ()>("sent")
    })
    .expect("sent");
    assert_eq!(sent, "sent");
    assert_eq!(waits(&list), [offered + 12 * MS]);
    assert_eq!(waits_at_send, Some(1), "the send runs after the wait");
    assert_eq!(
        timing,
        SendTiming {
            at_us: 12_000,
            late: false,
            started: offered + 12 * MS,
        }
    );
}

#[test]
fn a_compose_done_exactly_at_the_due_instant_is_not_late() {
    let offered = Instant::now();
    let (mut clock, list) = FakeClock::at(offered + 12 * MS);
    let (_, timing) =
        send_at(&mut clock, send_due(offered), offered, || Ok::<_, ()>(())).expect("sent");
    assert_eq!(waits(&list), [offered + 12 * MS]);
    assert_eq!(
        timing,
        SendTiming {
            at_us: 12_000,
            late: false,
            started: offered + 12 * MS,
        }
    );
}

#[test]
fn a_compose_past_the_due_instant_sends_at_once_and_is_late() {
    let offered = Instant::now();
    let (mut clock, list) = FakeClock::at(offered + 15 * MS);
    let (_, timing) =
        send_at(&mut clock, send_due(offered), offered, || Ok::<_, ()>(())).expect("sent");
    assert!(waits(&list).is_empty(), "no wait once the instant passed");
    assert_eq!(
        timing,
        SendTiming {
            at_us: 15_000,
            late: true,
            started: offered + 15 * MS,
        }
    );
}

#[test]
fn a_failed_send_is_the_callers_error() {
    let offered = Instant::now();
    let (mut clock, list) = FakeClock::at(offered);
    let out = send_at(&mut clock, send_due(offered), offered, || {
        Err::<(), _>("lost")
    });
    assert_eq!(out, Err("lost"));
    assert_eq!(
        waits(&list),
        [offered + 12 * MS],
        "it failed at its instant"
    );
}
