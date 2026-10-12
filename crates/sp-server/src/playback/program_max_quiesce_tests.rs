//! #243 follow-up: the deploy's quiesce holds a minute from its last ask,
//! then a thread the deploy did not stop serves again; a shutdown waits
//! for the thread's end, at most its bound. The decisions on injected
//! instants (`quiesce_at`, `try_next_at`), the wake-up and the shutdown
//! over a real `run_max_loop` on the fake GPU.
//! Wired via `#[cfg(test)] #[path = "program_max_quiesce_tests.rs"] mod tests;`.

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use super::{MAX_STOP_WAIT, QUIESCE_EXPIRY, stop_and_wait};
use crate::playback::program_max::{MaxJob, MaxNext, MaxOut};
use crate::playback::program_max_worker::tests::{
    FakeGpu, Gate, Hold, picture, plain, spawn_loop, wait_until,
};

const MS: Duration = Duration::from_millis(1);

fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

fn black(stamp: i64) -> MaxJob {
    MaxJob::Black { stamp_100ns: stamp }
}

/// MAX on, a thread attached: it takes jobs.
fn taking(max: &MaxOut) -> crate::playback::program_max::Consumer<'_> {
    max.set_enabled(true);
    max.attach()
}

#[test]
fn a_quiesce_holds_a_minute_and_a_shutdown_waits_three_seconds() {
    assert_eq!(QUIESCE_EXPIRY, secs(60));
    assert_eq!(MAX_STOP_WAIT, secs(3));
}

/// The quiesce, then the thread waits; a minute after the ask it resumes
/// and jobs are taken again.
#[test]
fn a_quiesce_holds_for_a_minute_then_the_thread_resumes() {
    let max = MaxOut::new();
    let _consumer = taking(&max);
    let t0 = Instant::now();
    assert!(max.quiesce_at(t0));
    assert!(matches!(max.try_next_at(false, t0), Some(MaxNext::Quiesce)));
    assert!(
        max.try_next_at(false, t0 + secs(60) - MS).is_none(),
        "it holds: the thread waits"
    );
    assert!(!max.accepting(), "nothing is offered while it holds");
    assert!(
        matches!(max.try_next_at(false, t0 + secs(60)), Some(MaxNext::Resume)),
        "a minute after the ask"
    );
    assert!(max.accepting(), "jobs are taken again");
    assert!(max.offer_with(|| black(1)));
    assert!(matches!(
        max.try_next_at(true, t0 + secs(61)),
        Some(MaxNext::Job(..))
    ));
}

/// A thread that sees the ask only after its end (stalled meanwhile)
/// still quiesces once, the senders dropped, then resumes.
#[test]
fn an_ask_seen_only_after_its_end_still_quiesces_once() {
    let max = MaxOut::new();
    let _consumer = taking(&max);
    let t0 = Instant::now();
    max.quiesce_at(t0);
    assert!(matches!(
        max.try_next_at(false, t0 + secs(61)),
        Some(MaxNext::Quiesce)
    ));
    assert!(matches!(
        max.try_next_at(false, t0 + secs(61)),
        Some(MaxNext::Resume)
    ));
}

/// An ask while the quiesce holds moves its end; once it ended, an ask is
/// a new quiesce, and the thread quiesces again.
#[test]
fn a_repeated_ask_renews_the_quiesce() {
    let max = MaxOut::new();
    let _consumer = taking(&max);
    let t0 = Instant::now();
    assert!(max.quiesce_at(t0));
    assert!(matches!(max.try_next_at(false, t0), Some(MaxNext::Quiesce)));
    assert!(!max.quiesce_at(t0 + secs(50)), "renewed, not new");
    assert!(
        max.try_next_at(false, t0 + secs(60)).is_none(),
        "renewed: it still holds"
    );
    assert!(max.try_next_at(false, t0 + secs(110) - MS).is_none());
    assert!(matches!(
        max.try_next_at(false, t0 + secs(110)),
        Some(MaxNext::Resume)
    ));
    assert!(max.quiesce_at(t0 + secs(120)), "a new quiesce");
    assert!(
        matches!(
            max.try_next_at(false, t0 + secs(120)),
            Some(MaxNext::Quiesce)
        ),
        "the thread quiesces again"
    );
}

/// A quiesced thread waiting for its next step wakes by itself at the
/// quiesce's end: nothing notifies it then.
#[test]
fn a_quiesced_thread_wakes_by_itself_at_the_end() {
    let max = Arc::new(MaxOut::new());
    let _consumer = taking(&max);
    max.set_quiesce_expiry(Duration::from_millis(200));
    let t0 = Instant::now();
    max.quiesce_at(t0);
    assert!(matches!(max.try_next_at(false, t0), Some(MaxNext::Quiesce)));
    let (step, step_rx) = mpsc::channel();
    let waiting = max.clone();
    std::thread::spawn(move || {
        let _ = step.send(waiting.next(false));
    });
    let step = step_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("it wakes at the end, with no notify");
    assert!(matches!(step, MaxNext::Resume), "{step:?}");
}

/// A deploy that did not kill the process (a cancelled run, a failed
/// kill): the real loop quiesces, then serves again once the quiesce
/// expired, the senders built anew.
#[test]
fn a_quiesced_loop_the_deploy_did_not_stop_serves_again() {
    let max = Arc::new(MaxOut::new());
    max.set_enabled(true);
    max.set_quiesce_expiry(Duration::from_millis(200));
    let gpu = FakeGpu::default();
    let thread = spawn_loop(&max, &gpu);
    wait_until("the loop takes jobs", || max.accepting());
    let a = picture(4, 2);
    max.offer_with(|| plain(1, &a));
    wait_until("the first boundary went out", || {
        max.status().submitted == 1
    });
    max.quiesce();
    wait_until("quiesced: the sender and the compositor dropped", || {
        gpu.log().drops.len() == 2
    });
    // Nothing wakes the thread at the deadline (a refused offer notifies
    // nobody): it wakes by itself. Offer until a job is taken again; it is
    // served on senders built anew.
    wait_until("the quiesce expired: a boundary went out again", || {
        max.offer_with(|| plain(2, &a));
        max.status().submitted >= 2
    });
    {
        let log = gpu.log();
        assert_eq!(
            (log.compositors_built, log.senders_built),
            (2, 2),
            "built anew"
        );
    }
    assert_eq!(max.status().state, "running");
    max.stop();
    wait_until("the loop exits on stop", || thread.is_finished());
    thread.join().expect("the loop");
}

/// No thread: nothing to wait for. A thread that does not end: the bound.
/// A thread that ends while the shutdown waits: woken by its end.
#[test]
fn a_shutdown_waits_for_the_thread_s_end_at_most_its_bound() {
    let max = Arc::new(MaxOut::new());
    assert!(max.wait_ended(secs(20)), "no thread: nothing to wait for");
    let consumer = max.attach();
    assert!(
        !max.wait_ended(Duration::from_millis(50)),
        "a thread that does not end: the bound"
    );
    let (ended, ended_rx) = mpsc::channel();
    let waiting = max.clone();
    std::thread::spawn(move || {
        let _ = ended.send(waiting.wait_ended(secs(20)));
    });
    assert!(
        ended_rx.recv_timeout(Duration::from_millis(100)).is_err(),
        "it waits while the thread runs"
    );
    drop(consumer);
    assert_eq!(
        ended_rx.recv_timeout(secs(20)),
        Ok(true),
        "woken by the end"
    );
}

/// The shutdown returns only once the thread dropped its senders: the
/// thread is held in a compose that a helper lets go 300 ms after the
/// shutdown began, so a wait that does not cover it finds them still held.
#[tokio::test]
async fn the_shutdown_returns_once_the_thread_dropped_its_senders() {
    let max = Arc::new(MaxOut::new());
    max.set_enabled(true);
    let gpu = FakeGpu::default();
    let thread = spawn_loop(&max, &gpu);
    wait_until("the loop takes jobs", || max.accepting());
    let gate = Arc::new(Gate::default());
    let (entered, entered_rx) = mpsc::channel();
    gpu.hold_next_compose(Hold {
        entered,
        gate: gate.clone(),
    });
    max.offer_with(|| plain(1, &picture(4, 2)));
    entered_rx
        .recv_timeout(secs(20))
        .expect("the compose is held");
    let opener = {
        let gate = gate.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            gate.open();
        })
    };
    assert!(
        stop_and_wait(max.clone(), secs(20)).await,
        "the thread ended within the bound"
    );
    assert_eq!(
        gpu.log().drops,
        ["sender", "compositor"],
        "both dropped before the shutdown went on"
    );
    opener.join().expect("the opener");
    wait_until("the loop thread is gone", || thread.is_finished());
    thread.join().expect("the loop");
}
