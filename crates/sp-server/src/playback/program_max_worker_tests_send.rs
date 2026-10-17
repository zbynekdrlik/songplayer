//! #223 follow-up: the `program-max` thread sends each boundary at its due
//! instant, whatever its compose cost — `MAX_SEND_LEAD` after the program
//! offered it, or on a slot of the wall's refresh grid when a source
//! measures it — on the fake GPU and a clock that never sleeps, and through
//! the real loop on the real clock. #251: on the grid the compose waits for
//! its own instant (the compose phase), never runs at the offer.
//! Wired via `#[cfg(test)] #[path = "program_max_worker_tests_send.rs"] mod tests_send;`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sp_gpu::{VblankGrid, VblankState};

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
    max.set_enabled(true);
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
    max.set_enabled(true);
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

/// A refresh grid the test sets (`None`: not measured — then its output
/// does not tick, #243).
struct FakeVblank(Option<VblankGrid>);

impl VblankSource for FakeVblank {
    fn grid(&self, _now: Instant) -> Option<VblankGrid> {
        self.0
    }

    fn state(&self, _now: Instant) -> VblankState {
        if self.0.is_some() {
            VblankState::Ticking
        } else {
            VblankState::NotTicking
        }
    }

    fn output(&self) -> String {
        "fake 7680x1080".to_string()
    }
}

/// On the wall's grid (a vblank 1 ms before the offer, 16 ms apart) with
/// the phase setting at 5 ms and the compose phase at its 13 ms default,
/// K = (5 − 13) mod 16 + 16 = 24 ms: the slots are 4 + 16k ms after the
/// offer, the boundary goes in the first from 24 ms on (36 ms), 5 ms after
/// its vblank, composed at 12 ms (13 ms after its vblank).
#[test]
fn on_a_refresh_grid_the_worker_sends_in_its_slot_at_the_phase_setting() {
    let max = MaxOut::new();
    max.set_enabled(true);
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

    assert_eq!(waits(&list), [offered + 12 * MS, offered + 36 * MS]);
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
        (true, 16_000_000, 0, 5_000, 0, 36_000)
    );
    assert_eq!(status.vblank_state, Some("ticking"));
}

/// #251 step 4: the grid the worker paces on is published for the
/// `SP-program` video side's quiet window (3–10.5 ms by default); with no
/// grid, none is.
#[test]
fn the_worker_publishes_its_grid_for_the_video_quiet_window() {
    let max = MaxOut::new();
    max.set_enabled(true);
    let offered = Instant::now();
    let grid = VblankGrid {
        at: offered - MS,
        period: 16 * MS,
    };
    let (clock, _list) = FakeClock::at(offered + 4 * MS);
    let mut worker = MaxWorker::new(&max, FakeGpu::default())
        .with_clock(Box::new(clock))
        .with_vblank(Box::new(FakeVblank(Some(grid))));
    assert_eq!(max.video.start_at(offered + 4 * MS), None, "none yet");
    worker.serve_offered(&job(1), offered, offered + 4 * MS);
    // 5 ms after its vblank: inside the window, wait to 10.5 ms.
    let end = offered + 9 * MS + MS / 2;
    assert_eq!(max.video.start_at(offered + 4 * MS), Some(end));
    assert!(max.video.status(offered + 4 * MS).grid);

    let max = MaxOut::new();
    max.set_enabled(true);
    let (clock, _list) = FakeClock::at(offered + 4 * MS);
    let mut worker = MaxWorker::new(&max, FakeGpu::default())
        .with_clock(Box::new(clock))
        .with_vblank(Box::new(FakeVblank(None)));
    worker.serve_offered(&job(1), offered, offered + 4 * MS);
    assert_eq!(max.video.start_at(offered + 4 * MS), None, "no grid");
    assert!(!max.video.status(offered + 4 * MS).grid);
}

/// The wall's grid of the test above (a vblank 1 ms before the offer,
/// 16 ms apart), the send phase 5 ms.
fn grid_test_worker<'a>(
    max: &'a MaxOut,
    gpu: &FakeGpu,
    offered: Instant,
    now: Instant,
) -> (MaxWorker<'a, FakeGpu>, Waits, Snapshots) {
    max.set_vblank_phase_us(5_000);
    let grid = VblankGrid {
        at: offered - MS,
        period: 16 * MS,
    };
    let (mut clock, list) = FakeClock::at(now);
    let at_wait = Arc::new(Mutex::new(Vec::new()));
    let (seen, gpu_then) = (at_wait.clone(), gpu.clone());
    clock.on_wait = Some(Box::new(move || {
        let log = gpu_then.log();
        seen.lock()
            .unwrap_or_else(|p| p.into_inner())
            .push((log.drawn.len(), log.sent));
    }));
    let worker = MaxWorker::new(max, gpu.clone())
        .with_clock(Box::new(clock))
        .with_vblank(Box::new(FakeVblank(Some(grid))));
    (worker, list, at_wait)
}

/// The instants the fake clock waited for.
type Waits = Arc<Mutex<Vec<Instant>>>;

/// What the fake GPU had drawn and sent at each wait.
type Snapshots = Arc<Mutex<Vec<(usize, u32)>>>;

fn snapshots(at_wait: &Snapshots) -> Vec<(usize, u32)> {
    at_wait.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// #251, the bug: the compose (the 4K uploads and the blend draw) ran at
/// the offer, which drifts against the refresh, so for minutes at a time it
/// landed where Arena renders and reads Spout. On the grid it now waits for
/// its own instant, 13 ms after a vblank (K = 24 ms before the send):
/// nothing is drawn before that wait, the boundary is drawn before the
/// send's.
#[test]
fn on_a_grid_the_compose_waits_for_its_phase_never_runs_at_the_offer() {
    let max = MaxOut::new();
    max.set_enabled(true);
    let gpu = FakeGpu::default();
    let offered = Instant::now();
    let now = offered + 4 * MS;
    let (mut worker, list, at_wait) = grid_test_worker(&max, &gpu, offered, now);
    worker.serve_offered(&job(1), offered, now);

    assert_eq!(waits(&list), [offered + 12 * MS, offered + 36 * MS]);
    assert_eq!(
        snapshots(&at_wait),
        [(0, 0), (1, 0)],
        "composed after the compose wait, sent after the send's"
    );
    let status = max.status();
    assert_eq!(
        (
            status.compose_phase_us,
            status.compose_phase_us_p50,
            status.compose_phase_us_p99,
            status.compose_late,
            status.send_late
        ),
        (13_000, 13_000, 13_000, 0, 0)
    );
}

/// #251: a job taken after its compose instant (the thread was still busy
/// with the boundary before) composes at once and is counted
/// `compose_late`; its send keeps its slot.
#[test]
fn a_job_taken_after_its_compose_instant_composes_at_once_and_counts_it() {
    let max = MaxOut::new();
    max.set_enabled(true);
    let gpu = FakeGpu::default();
    let offered = Instant::now();
    let now = offered + 14 * MS;
    let (mut worker, list, at_wait) = grid_test_worker(&max, &gpu, offered, now);
    worker.serve_offered(&job(1), offered, now);

    assert_eq!(
        waits(&list),
        [offered + 36 * MS],
        "no compose wait: its instant, 12 ms, had passed"
    );
    assert_eq!(snapshots(&at_wait), [(1, 0)]);
    let status = max.status();
    assert_eq!(
        (
            status.compose_late,
            status.compose_phase_us_p50,
            status.send_late,
            status.send_phase_us_p50
        ),
        (1, 15_000, 0, 5_000)
    );
}

/// #251: off the grid (a dark panel: PP's laptop) nothing changes — the
/// compose runs at once, the send waits for the constant lead, and no
/// compose phase is recorded.
#[test]
fn off_the_grid_the_compose_runs_at_once() {
    let max = MaxOut::new();
    max.set_enabled(true);
    let gpu = FakeGpu::default();
    let offered = Instant::now();
    let (mut clock, list) = FakeClock::at(offered + 4 * MS);
    let at_wait: Snapshots = Arc::new(Mutex::new(Vec::new()));
    let (seen, gpu_then) = (at_wait.clone(), gpu.clone());
    clock.on_wait = Some(Box::new(move || {
        let log = gpu_then.log();
        seen.lock()
            .unwrap_or_else(|p| p.into_inner())
            .push((log.drawn.len(), log.sent));
    }));
    let mut worker = MaxWorker::new(&max, gpu.clone())
        .with_clock(Box::new(clock))
        .with_vblank(Box::new(FakeVblank(None)));
    worker.serve_offered(&job(1), offered, offered + 4 * MS);

    assert_eq!(waits(&list), [offered + 12 * MS]);
    assert_eq!(
        snapshots(&at_wait),
        [(1, 0)],
        "composed before the only wait"
    );
    let status = max.status();
    assert_eq!(
        (
            status.compose_late,
            status.compose_phase_us_p50,
            status.send_off_grid
        ),
        (0, 0, 1)
    );
}

/// #251, the phase-lucky rule (`rust-workspace.md`): offers at three
/// distinct phases of SNV's refresh (16.666 667 ms, send phase 19 ms =
/// 2.333 ms into it, compose phase 13 ms) each compose exactly 13 ms after
/// a vblank and send 2.333 ms after one, K = 22.667 ms later.
#[test]
fn the_compose_lands_on_its_phase_whatever_the_offers_phase() {
    let period = Duration::from_nanos(16_666_667);
    let at = Instant::now();
    for offset_us in [0u64, 5_500, 11_100] {
        let max = MaxOut::new();
        max.set_enabled(true);
        max.set_vblank_phase_us(19_000);
        let gpu = FakeGpu::default();
        let offered = at + period * 10 + Duration::from_micros(offset_us);
        let now = offered + Duration::from_micros(200);
        let (clock, list) = FakeClock::at(now);
        let mut worker = MaxWorker::new(&max, gpu.clone())
            .with_clock(Box::new(clock))
            .with_vblank(Box::new(FakeVblank(Some(VblankGrid { at, period }))));
        worker.serve_offered(&job(1), offered, now);

        let waited = waits(&list);
        assert_eq!(waited.len(), 2, "{offset_us}: a compose and a send wait");
        assert_eq!(
            waited[1] - waited[0],
            Duration::from_nanos(22_666_667),
            "{offset_us}"
        );
        let status = max.status();
        assert_eq!(
            (
                status.compose_phase_us_p50,
                status.send_phase_us_p50,
                status.compose_late
            ),
            (13_000, 2_333, 0),
            "{offset_us}"
        );
    }
}

/// A source that measures nothing yet: the constant lead, off the grid.
#[test]
fn an_unmeasured_refresh_sends_at_the_constant_lead() {
    let max = MaxOut::new();
    max.set_enabled(true);
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
    assert_eq!(
        status.vblank_state,
        Some("not_ticking"),
        "#243: the source's state is recorded at each boundary"
    );
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
        .spawn(move || run_max_loop(&out, loop_gpu, Some(Box::new(source)), None))
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

/// A refresh source whose output the test moves (#259: the vblank thread
/// moves to the new primary when Windows re-lays the displays).
struct MovingVblank(Arc<Mutex<String>>);

impl VblankSource for MovingVblank {
    fn grid(&self, _now: Instant) -> Option<VblankGrid> {
        None
    }

    fn state(&self, _now: Instant) -> VblankState {
        VblankState::Measuring
    }

    fn output(&self) -> String {
        self.0.lock().unwrap().clone()
    }
}

/// #259: the telemetry's `vblank_output` names the output the source
/// measures NOW, not the one it was given with (SNV 13.10.2026: the VDD
/// was detached and the span became the primary for a minute).
#[test]
fn the_paced_output_is_named_as_it_moves() {
    let max = MaxOut::new();
    max.set_enabled(true);
    let gpu = FakeGpu::default();
    let name = Arc::new(Mutex::new(r"\\.\DISPLAY5 3840x2160".to_string()));
    let offered = Instant::now();
    let (clock, _list) = FakeClock::at(offered + 4 * MS);
    let mut worker = MaxWorker::new(&max, gpu.clone())
        .with_clock(Box::new(clock))
        .with_vblank(Box::new(MovingVblank(name.clone())));
    assert_eq!(
        max.status().vblank_output.as_deref(),
        Some(r"\\.\DISPLAY5 3840x2160")
    );
    *name.lock().unwrap() = r"\\.\DISPLAY1 7680x1080".to_string();
    worker.serve_offered(&job(1), offered, offered + 4 * MS);
    assert_eq!(
        max.status().vblank_output.as_deref(),
        Some(r"\\.\DISPLAY1 7680x1080"),
        "named at the boundary after the move"
    );
}
