//! #223 follow-up: the `program-max` thread sends each boundary at its due
//! instant, whatever its compose cost — `MAX_SEND_LEAD` after the program
//! offered it, or on a slot of the wall's refresh grid when a source
//! measures it — on the fake GPU and a clock that never sleeps, and through
//! the real loop on the real clock.
//! Wired via `#[cfg(test)] #[path = "program_max_worker_tests_send.rs"] mod tests_send;`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sp_gpu::VblankGrid;

use super::tests::{FakeGpu, picture, spawn_loop, wait_until};
use super::{MaxWorker, run_max_loop};
use crate::playback::program_max::{MaxJob, MaxOut};
use crate::playback::program_max_send::tests::{FakeClock, waits};
use crate::playback::program_max_vblank::VblankSource;

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

/// A refresh grid the test sets (`None`: not measured).
struct FakeVblank(Option<VblankGrid>);

impl VblankSource for FakeVblank {
    fn grid(&self, _now: Instant) -> Option<VblankGrid> {
        self.0
    }

    fn output(&self) -> String {
        "fake 7680x1080".to_string()
    }
}

/// On the wall's grid (a vblank 1 ms before the offer, 16 ms apart) with
/// the phase setting at 5 ms, the slots are 4 + 16k ms after the offer: the
/// boundary goes in the first from 12 ms on (20 ms), 5 ms after its vblank.
#[test]
fn on_a_refresh_grid_the_worker_sends_in_its_slot_at_the_phase_setting() {
    let max = MaxOut::new();
    max.set_vblank_phase_us(5_000);
    let gpu = FakeGpu::default();
    let offered = Instant::now();
    let grid = VblankGrid {
        at: offered - MS,
        period: 16 * MS,
    };
    let (clock, list) = FakeClock::at(offered + 4 * MS);
    let mut worker = MaxWorker::new(&max, gpu.clone())
        .with_clock(Box::new(clock))
        .with_vblank(Box::new(FakeVblank(Some(grid))));
    assert_eq!(
        max.status().vblank_output.as_deref(),
        Some("fake 7680x1080"),
        "named when given"
    );
    worker.serve_offered(&job(1), offered, offered + 4 * MS);

    assert_eq!(waits(&list), [offered + 20 * MS]);
    assert_eq!(gpu.log().sent, 1);
    let status = max.status();
    assert_eq!(
        (
            status.vblank_tracking,
            status.vblank_period_ns,
            status.send_off_grid,
            status.send_phase_us_p50,
            status.slot_repicks,
            status.send_at_us_p50
        ),
        (true, 16_000_000, 0, 5_000, 0, 20_000)
    );
}

/// A source that measures nothing yet: the constant lead, off the grid.
#[test]
fn an_unmeasured_refresh_sends_at_the_constant_lead() {
    let max = MaxOut::new();
    let gpu = FakeGpu::default();
    let offered = Instant::now();
    let (clock, list) = FakeClock::at(offered + 4 * MS);
    let mut worker = MaxWorker::new(&max, gpu.clone())
        .with_clock(Box::new(clock))
        .with_vblank(Box::new(FakeVblank(None)));
    worker.serve_offered(&job(1), offered, offered + 4 * MS);

    assert_eq!(waits(&list), [offered + 12 * MS]);
    let status = max.status();
    assert_eq!((status.vblank_tracking, status.send_off_grid), (false, 1));
    assert_eq!(status.vblank_output.as_deref(), Some("fake 7680x1080"));
}

/// The loop given a refresh source sends on its grid (real clock: only
/// that it went on the grid, and never before 12 ms after the offer).
#[test]
fn the_loop_sends_on_the_refresh_grid_it_is_given() {
    let max = Arc::new(MaxOut::new());
    max.set_enabled(true);
    let gpu = FakeGpu::default();
    let source = FakeVblank(Some(VblankGrid {
        at: Instant::now(),
        period: 16 * MS,
    }));
    let (out, loop_gpu) = (max.clone(), gpu.clone());
    let thread = std::thread::Builder::new()
        .name("program-max-test".into())
        .spawn(move || run_max_loop(&out, loop_gpu, Some(Box::new(source))))
        .expect("spawn the loop");
    wait_until("the loop takes jobs", || max.accepting());
    max.offer_with(|| job(1));
    wait_until("the job went out", || max.status().submitted == 1);
    let status = max.status();
    assert_eq!(status.vblank_output.as_deref(), Some("fake 7680x1080"));
    assert_eq!((status.vblank_tracking, status.send_off_grid), (true, 0));
    assert!(
        status.send_at_us_p50 >= 12_000,
        "sent {} us after the offer, before the lead window",
        status.send_at_us_p50
    );
    max.stop();
    wait_until("the loop exits on stop", || thread.is_finished());
    thread.join().expect("the loop");
}
