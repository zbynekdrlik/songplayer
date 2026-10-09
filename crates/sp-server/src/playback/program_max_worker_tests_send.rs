//! #223 follow-up: the `program-max` thread sends each boundary at a
//! constant phase, `MAX_SEND_LEAD` after the program offered it, whatever
//! its compose cost — on the fake GPU and a clock that never sleeps, and
//! once through the real loop on the real clock.
//! Wired via `#[cfg(test)] #[path = "program_max_worker_tests_send.rs"] mod tests_send;`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::MaxWorker;
use super::tests::{FakeGpu, picture, spawn_loop, wait_until};
use crate::playback::program_max::{MaxJob, MaxOut};
use crate::playback::program_max_send::tests::{FakeClock, waits};

const MS: Duration = Duration::from_millis(1);

fn job(stamp: i64) -> MaxJob {
    MaxJob::Picture {
        stamp_100ns: stamp,
        picture: picture(4, 2),
    }
}

/// The bug: `SendTexture` ran the moment the compose was done, so each
/// boundary left at the offer + a compose time that varies by ms and
/// Arena's 60 Hz render showed it for 1 or 3 frames. Now the compose runs
/// at once and the send waits for the due instant.
#[test]
fn the_worker_composes_at_once_and_sends_at_the_due_instant() {
    let max = MaxOut::new();
    let gpu = FakeGpu::default();
    let offered = Instant::now();
    let (mut clock, list) = FakeClock::at(offered + 4 * MS);
    let at_wait = Arc::new(Mutex::new(Vec::new()));
    let (seen, gpu_then) = (at_wait.clone(), gpu.clone());
    clock.on_wait = Some(Box::new(move || {
        let log = gpu_then.log();
        seen.lock()
            .unwrap_or_else(|p| p.into_inner())
            .push((log.drawn.len(), log.sent));
    }));
    let mut worker = MaxWorker::new(&max, gpu.clone()).with_clock(Box::new(clock));
    worker.serve_offered(&job(1), offered, offered + 4 * MS);

    assert_eq!(
        waits(&list),
        [offered + 12 * MS],
        "it waited for the due instant"
    );
    assert_eq!(
        *at_wait.lock().unwrap_or_else(|p| p.into_inner()),
        [(1, 0)],
        "composed before the wait, sent after it"
    );
    assert_eq!(gpu.log().sent, 1);
    let status = max.status();
    assert_eq!(status.submitted, 1);
    assert_eq!(
        (
            status.send_at_us_p50,
            status.send_at_us_max,
            status.send_late
        ),
        (12_000, 12_000, 0)
    );
}

#[test]
fn a_compose_that_ran_past_the_due_instant_is_sent_at_once_and_counted_late() {
    let max = MaxOut::new();
    let gpu = FakeGpu::default();
    let offered = Instant::now();
    let (clock, list) = FakeClock::at(offered + 20 * MS);
    let mut worker = MaxWorker::new(&max, gpu.clone()).with_clock(Box::new(clock));
    worker.serve_offered(&job(1), offered, offered + 20 * MS);

    assert!(waits(&list).is_empty(), "no wait once the instant passed");
    assert_eq!(gpu.log().sent, 1);
    let status = max.status();
    assert_eq!((status.send_at_us_max, status.send_late), (20_000, 1));
}

/// The loop runs on the real clock: a boundary never leaves before its due
/// instant (a lower bound only: a stalled runner can only make it later).
#[test]
fn the_loop_sends_no_boundary_before_its_due_instant() {
    let max = Arc::new(MaxOut::new());
    max.set_enabled(true);
    let gpu = FakeGpu::default();
    let thread = spawn_loop(&max, &gpu);
    wait_until("the loop takes jobs", || max.accepting());
    max.offer_with(|| job(1));
    wait_until("the job went out", || max.status().submitted == 1);
    let status = max.status();
    assert!(
        status.send_at_us_p50 >= 12_000,
        "sent {} us after the offer, before its 12 ms phase",
        status.send_at_us_p50
    );
    max.stop();
    wait_until("the loop exits on stop", || thread.is_finished());
    thread.join().expect("the loop");
}
