//! #239 + #251 step 2 + #279 P1b: the `SP-program` Spout sender on the
//! `program-max` thread, on the fake GPU. Its target is the compositor's
//! PRIMARY (the FHD program, drawn by every compose); MAX's target is added
//! with MAX's sender. The FHD sender is built only while it is wanted,
//! AFTER MAX's send (MAX goes first, whole); from the next boundary one
//! compose at the compose instant draws both targets from the same
//! pictures, MAX is sent at the due instant and the FHD target right after
//! it. Its sender keeps its own refusals, backoff, telemetry and log, never
//! MAX's; a lost device is one event for both; switched off or refused,
//! nothing is sent for it (its target, the primary, stays).
//! Wired via `#[cfg(test)] #[path = "program_max_worker_tests_fhd.rs"] mod tests_fhd;`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sp_core::config::SpoutSend;
use sp_gpu::{GpuError, MAX_TARGET, VblankGrid, VblankState};

use super::tests::{
    DRAW_US, Drawn, FAKE_LISTED, FHD_DRAW_US, FHD_SEND_US, FakeGpu, SEND_US, UPLOAD_US,
    not_registered, pic, picture, plain, spawn_loop, wait_until,
};
use super::{LogLine, MAX_RETRY_BACKOFF, MaxWorker};
use crate::playback::program_max::{FHD_OFF_SETTING, MaxJob, MaxOut};
use crate::playback::program_max_send::tests::{FakeClock, waits};
use crate::playback::program_max_vblank::VblankSource;

const MS: Duration = Duration::from_millis(1);

/// MAX on and the FHD sender's setting on.
pub(super) fn fhd_on() -> MaxOut {
    let max = MaxOut::new();
    max.set_enabled(true);
    max.set_fhd_enabled(true);
    max
}

pub(super) fn black(stamp: i64) -> MaxJob {
    MaxJob::Black { stamp_100ns: stamp }
}

/// One boundary on both outputs, in order: one compose draws both targets
/// (the FHD primary first), then MAX's send, then the FHD one's.
const BOUNDARY: [&str; 4] = ["fhd draw", "max draw", "max send", "fhd send"];

/// A boundary of MAX alone (the FHD sender not built yet, or off): the
/// compose still draws the primary.
const MAX_ALONE: [&str; 3] = ["fhd draw", "max draw", "max send"];

/// The FHD output's build: its sender of the compositor's primary.
const FHD_BUILD: [&str; 1] = ["fhd sender built"];

/// A refused Spout name at create (a refusal like `not_registered`).
pub(super) fn name_taken() -> GpuError {
    GpuError::SpoutNameTaken {
        name: "SP-program".into(),
    }
}

/// One lost Spout frame (no refusal, no lost device).
fn lost_frame() -> GpuError {
    GpuError::Spout {
        call: "spout_sender_send",
        code: 3,
    }
}

/// #239: MAX goes first, whole: the FHD output is built AFTER MAX's send
/// (its Spout open never delays this boundary's MAX, at most the next
/// compose: `compose_late`) and drawn from the next compose. From then on ONE compose draws both targets from the same
/// pictures, and MAX is sent before the FHD target.
#[test]
fn the_fhd_output_is_built_after_max_s_send_then_drawn_by_the_same_compose() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let (a, b) = (picture(4, 2), picture(8, 2));
    let t0 = Instant::now();
    worker.serve(&plain(1, &a), t0);
    let fade = MaxJob::Fade {
        stamp_100ns: 2,
        from: Some(a.clone()),
        to: Some(b.clone()),
        weight_q8: 64,
    };
    worker.serve(&fade, t0);
    worker.serve(&black(3), t0);
    assert!(worker.holds_gpu());
    let log = gpu.log();
    assert_eq!(log.compositors_built, 1, "ONE compositor for both targets");
    assert_eq!(log.targets_added, [MAX_TARGET], "MAX's target, added once");
    assert_eq!(
        (log.senders_built, log.fhd_senders_built),
        (1, 1),
        "one sender per target"
    );
    let drawn = [
        Drawn::Picture(pic(1, &a)),
        Drawn::Fade(Some(pic(1, &a)), Some(pic(2, &b)), 64),
        Drawn::Black,
    ];
    assert_eq!(log.drawn, drawn, "one compose per boundary");
    assert_eq!(
        log.fhd_drawn, drawn,
        "the FHD target, the primary, drawn by the same composes: the same pictures, ids and weight"
    );
    let mut want = MAX_ALONE.to_vec();
    want.extend(FHD_BUILD);
    want.extend(BOUNDARY);
    want.extend(BOUNDARY);
    assert_eq!(
        log.order, want,
        "MAX whole first, the FHD build after its send, then both"
    );
    assert_eq!(log.listed_reads, 1, "the registry is read once");
    drop(log);
    let status = max.status();
    assert_eq!((status.submitted, status.failed), (3, 0));
    assert_eq!(
        (status.upload_us_p99, status.draw_us_p99, status.send_us_p99),
        (UPLOAD_US, DRAW_US, SEND_US),
        "MAX's: the one upload and the whole compose's draw"
    );
    let fhd = status.fhd;
    assert_eq!(
        (fhd.state.as_str(), fhd.reason, fhd.submitted, fhd.failed),
        ("running", None, 2, 0),
        "the build's boundary is neither sent nor failed"
    );
    assert_eq!(
        (fhd.draw_us_p99, fhd.send_us_p99),
        (FHD_DRAW_US, FHD_SEND_US),
        "the FHD target's own draw (never MAX's share) and its send"
    );
    assert_eq!((fhd.listed_width, fhd.listed_height), FAKE_LISTED);
}

/// A refresh grid the test sets.
struct Grid(VblankGrid);

impl VblankSource for Grid {
    fn grid(&self, _now: Instant) -> Option<VblankGrid> {
        Some(self.0)
    }

    fn state(&self, _now: Instant) -> VblankState {
        VblankState::Ticking
    }

    fn output(&self) -> String {
        "fake 7680x1080".to_string()
    }
}

/// What the fake GPU had done at each of the clock's waits.
type Orders = Arc<Mutex<Vec<Vec<&'static str>>>>;

/// The instants the fake clock waited for.
type Waits = Arc<Mutex<Vec<Instant>>>;

/// A worker on the fake clock at `now`, its waits recorded, and the fake's
/// order at each wait.
fn on_clock<'a>(
    max: &'a MaxOut,
    gpu: &FakeGpu,
    now: Instant,
) -> (MaxWorker<'a, FakeGpu>, Waits, Orders) {
    let (mut clock, list) = FakeClock::at(now);
    let at_wait: Orders = Arc::new(Mutex::new(Vec::new()));
    let (seen, gpu_then) = (at_wait.clone(), gpu.clone());
    clock.on_wait = Some(Box::new(move || {
        let order = gpu_then.log().order.clone();
        seen.lock().unwrap_or_else(|p| p.into_inner()).push(order);
    }));
    let worker = MaxWorker::new(max, gpu.clone()).with_clock(Box::new(clock));
    (worker, list, at_wait)
}

fn orders(at_wait: &Orders) -> Vec<Vec<&'static str>> {
    at_wait.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// Forget what the clock and the fake recorded so far (the boundary that
/// built the FHD output).
fn clear(list: &Waits, at_wait: &Orders, gpu: &FakeGpu) {
    list.lock().unwrap_or_else(|p| p.into_inner()).clear();
    at_wait.lock().unwrap_or_else(|p| p.into_inner()).clear();
    gpu.log().order.clear();
}

/// #251 step 2, the fix: on the grid (a vblank 1 ms before the first offer,
/// 16 ms apart, send phase 5 ms, compose phase 13 ms, K = 24 ms) BOTH
/// targets are composed at the compose instant, in one compose; at the due
/// instant MAX's send runs, then the FHD one's: plain copies, no GPU work
/// of the FHD side left for the send window.
#[test]
fn both_targets_are_composed_at_the_compose_instant_then_sent_max_first() {
    let max = fhd_on();
    max.set_vblank_phase_us(5_000);
    let gpu = FakeGpu::default();
    let offered = Instant::now();
    let (worker, list, at_wait) = on_clock(&max, &gpu, offered + 4 * MS);
    let grid = VblankGrid {
        at: offered - MS,
        period: 16 * MS,
    };
    let mut worker = worker.with_vblank(Box::new(Grid(grid)));
    worker.serve_offered(&black(1), offered, offered + 4 * MS);
    let mut built = MAX_ALONE.to_vec();
    built.extend(FHD_BUILD);
    assert_eq!(
        gpu.log().order,
        built,
        "the FHD output built after MAX's send"
    );
    clear(&list, &at_wait, &gpu);

    let next = offered + 33 * MS;
    worker.serve_offered(&black(2), next, next + MS);
    let waited = waits(&list);
    assert_eq!(waited.len(), 3, "{waited:?}");
    assert_eq!(waited[1] - waited[0], 24 * MS, "composed K before the send");
    assert_eq!(
        waited[2], waited[1],
        "the FHD send waits for no instant past MAX's"
    );
    let drawn = vec!["fhd draw", "max draw"];
    let mut max_sent = drawn.clone();
    max_sent.push("max send");
    assert_eq!(
        orders(&at_wait),
        [vec![], drawn, max_sent],
        "nothing drawn before the compose instant; both targets drawn before the sends"
    );
    let status = max.status();
    assert_eq!(
        (
            status.compose_phase_us_p50,
            status.send_phase_us_p50,
            status.compose_late,
            status.send_late
        ),
        (13_000, 5_000, 0, 0)
    );
    assert_eq!((status.submitted, status.fhd.submitted), (2, 1));
}

/// Off the grid the compose runs at once and MAX is sent at its due
/// instant; the FHD send follows at once (its due instant has passed).
#[test]
fn off_the_grid_the_fhd_send_follows_max_s_at_the_due_instant() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    let offered = Instant::now();
    let (mut worker, list, at_wait) = on_clock(&max, &gpu, offered + 4 * MS);
    worker.serve_offered(&black(1), offered, offered + 4 * MS);
    clear(&list, &at_wait, &gpu);

    let next = offered + 33 * MS;
    worker.serve_offered(&black(2), next, next + MS);
    let due = next + 12 * MS;
    assert_eq!(waits(&list), [due, due], "no FHD wait past MAX's instant");
    let drawn = vec!["fhd draw", "max draw"];
    let mut max_sent = drawn.clone();
    max_sent.push("max send");
    assert_eq!(orders(&at_wait), [drawn, max_sent]);
    let status = max.status();
    assert_eq!(
        (status.send_at_us_max, status.send_late),
        (12_000, 0),
        "MAX at its due instant, on time"
    );
}

/// The FHD build (a Spout open) runs AFTER MAX's send: MAX's boundary is
/// sent at its due instant before the FHD output even exists.
#[test]
fn the_fhd_build_never_delays_max_s_send() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    let offered = Instant::now();
    let (mut worker, list, at_wait) = on_clock(&max, &gpu, offered + 4 * MS);
    worker.serve_offered(&black(1), offered, offered + 4 * MS);
    assert_eq!(waits(&list), [offered + 12 * MS], "only MAX's send waited");
    assert_eq!(
        orders(&at_wait),
        [vec!["fhd draw", "max draw"]],
        "nothing of the FHD sender before MAX's send"
    );
    let mut want = MAX_ALONE.to_vec();
    want.extend(FHD_BUILD);
    assert_eq!(gpu.log().order, want);
    let status = max.status();
    assert_eq!((status.send_at_us_max, status.send_late), (12_000, 0));
    assert_eq!((status.fhd.submitted, status.fhd.failed), (0, 0));
}

/// MAX's sender refused at create (its own backoff; its target taken off
/// with it) never holds the FHD target back: with MAX's boundary not drawn
/// the FHD output is built first, the primary is composed, and it goes out
/// at the due instant, boundary after boundary, while MAX waits.
#[test]
fn with_max_s_sender_refused_the_fhd_target_still_goes_out_at_the_due_instant() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    gpu.script().sender.push_back(name_taken());
    let offered = Instant::now();
    let (mut worker, list, _) = on_clock(&max, &gpu, offered + 4 * MS);
    worker.serve_offered(&black(1), offered, offered + 4 * MS);

    assert_eq!(
        waits(&list),
        [offered + 12 * MS],
        "the FHD send's due instant"
    );
    let mut want = vec!["max target removed"];
    want.extend(FHD_BUILD);
    want.extend(["fhd draw", "fhd send"]);
    assert_eq!(gpu.log().order, want);
    let status = max.status();
    assert_eq!(
        (status.state, status.sender_backoffs, status.failed),
        (format!("error: {}", name_taken()), 1, 1)
    );
    assert_eq!(
        (
            status.fhd.state.as_str(),
            status.fhd.submitted,
            status.fhd.sender_backoffs
        ),
        ("running", 1, 0),
        "MAX's refusal is MAX's alone"
    );
    let later = offered + 40 * MS;
    worker.serve_offered(&black(2), later, later);
    let log = gpu.log();
    assert_eq!((log.senders_built, log.fhd_sent, log.sent), (1, 2, 0));
    drop(log);
    assert_eq!(max.status().failed, 2, "MAX skipped inside its backoff");
}

/// One lost MAX frame (no refusal, no lost device) costs MAX that boundary
/// only: the FHD target still goes out.
#[test]
fn a_lost_max_frame_never_stops_the_fhd_send() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&black(1), t0);
    gpu.script().send.push_back(lost_frame());
    worker.serve(&black(2), t0);
    let log = gpu.log();
    assert_eq!((log.sent, log.fhd_sent), (1, 1));
    assert!(log.drops.is_empty(), "nothing dropped");
    drop(log);
    let status = max.status();
    assert_eq!((status.failed, status.fhd.submitted), (1, 1));
}

/// The compositor is the device both outputs share: its failed build fails
/// both with the same reason, adds no FHD target, and both wait one backoff.
#[test]
fn a_failed_compositor_build_fails_both_outputs_and_waits_one_backoff() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    gpu.script().compositor.push_back(GpuError::NoAdapter);
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&black(1), t0);
    let why = format!("error: {}", GpuError::NoAdapter);
    let status = max.status();
    assert_eq!((status.state.as_str(), status.failed), (why.as_str(), 1));
    assert_eq!(
        (status.fhd.state.as_str(), status.fhd.failed),
        (why.as_str(), 1)
    );
    assert!(gpu.log().targets_added.is_empty());
    worker.serve(&black(2), t0 + MAX_RETRY_BACKOFF - MS);
    assert_eq!(gpu.log().compositors_built, 1, "inside the backoff");
    let status = max.status();
    assert_eq!((status.failed, status.fhd.failed), (2, 2), "both skipped");
    worker.serve(&black(3), t0 + MAX_RETRY_BACKOFF);
    worker.serve(&black(4), t0 + MAX_RETRY_BACKOFF);
    let log = gpu.log();
    assert_eq!((log.compositors_built, log.sent, log.fhd_sent), (2, 2, 1));
}

/// #279 P1b: MAX's target that cannot be added fails MAX's build (its own
/// backoff, never a refusal) and never stops the FHD output; nothing is
/// drawn for MAX until its backoff ends.
#[test]
fn a_failed_max_target_waits_max_s_own_backoff_and_never_stops_the_fhd_output() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    let failed = GpuError::Api {
        call: "CreateTexture2D",
        hresult: 0x8007_000E,
    };
    gpu.script().add_target.push_back(failed.clone());
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&black(1), t0);
    let status = max.status();
    assert_eq!(status.state, format!("error: {failed}"));
    assert_eq!((status.failed, status.sender_backoffs), (1, 0));
    assert_eq!(
        (status.fhd.state.as_str(), status.fhd.submitted),
        ("running", 1),
        "the FHD output built first and sent alone"
    );
    worker.serve(&black(2), t0 + MAX_RETRY_BACKOFF - MS);
    let log = gpu.log();
    assert_eq!(log.targets_added.len(), 1, "inside MAX's backoff");
    assert!(log.drawn.is_empty(), "no MAX target, nothing drawn for it");
    assert_eq!(log.fhd_sent, 2);
    drop(log);
    assert_eq!(max.status().failed, 2, "a skipped boundary counts");
    worker.serve(&black(3), t0 + MAX_RETRY_BACKOFF);
    let log = gpu.log();
    assert_eq!(log.targets_added, [MAX_TARGET, MAX_TARGET], "added again");
    assert_eq!((log.compositors_built, log.sent, log.fhd_sent), (1, 1, 3));
    drop(log);
    let status = max.status();
    assert_eq!(
        (status.state.as_str(), status.fhd.state.as_str()),
        ("running", "running")
    );
}

/// A failed build of the FHD sender (not a refusal) waits its own backoff;
/// MAX goes out meanwhile, and the primary stays on the compositor.
#[test]
fn a_failed_fhd_sender_build_waits_its_own_backoff_while_max_goes_out() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    gpu.script().fhd_sender.push_back(lost_frame());
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&black(1), t0);
    worker.serve(&black(2), t0 + MS);
    let log = gpu.log();
    assert_eq!(
        (log.fhd_senders_built, log.fhd_sent, log.targets_removed),
        (1, 0, 0),
        "no second build inside its backoff, nothing sent, nothing removed"
    );
    assert_eq!(log.sent, 2, "MAX went out");
    drop(log);
    let fhd = max.status().fhd;
    assert_eq!((fhd.failed, fhd.sender_backoffs), (2, 0), "not a refusal");
}

#[test]
fn a_refused_fhd_sender_is_dropped_and_a_new_one_waits_its_own_backoff() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&black(1), t0);
    worker.serve(&black(2), t0);
    assert_eq!(max.fhd_listed(), Some(FAKE_LISTED));
    gpu.script().fhd_send.push_back(not_registered());
    worker.serve(&black(3), t0);
    let log = gpu.log();
    assert_eq!(log.drops, ["fhd sender"], "the compositor stays");
    assert_eq!(log.targets_removed, 0, "its target, the primary, stays");
    drop(log);
    let status = max.status();
    assert_eq!(
        (status.fhd.sender_backoffs, status.sender_backoffs),
        (1, 0),
        "the FHD sender's, never MAX's"
    );
    assert_eq!(
        (status.fhd.listed_width, status.fhd.listed_height),
        (0, 0),
        "a dropped sender is listed nowhere"
    );
    worker.serve(&black(4), t0 + MAX_RETRY_BACKOFF - MS);
    let log = gpu.log();
    assert_eq!(log.fhd_senders_built, 1, "inside its backoff");
    assert_eq!(log.fhd_sent, 1, "nothing sent for it meanwhile");
    drop(log);
    worker.serve(&black(5), t0 + MAX_RETRY_BACKOFF);
    worker.serve(&black(6), t0 + MAX_RETRY_BACKOFF);
    let log = gpu.log();
    assert_eq!(
        (
            log.compositors_built,
            log.targets_added.len(),
            log.fhd_senders_built
        ),
        (1, 1, 2),
        "a new sender on the same compositor (MAX's target added once)"
    );
    assert_eq!((log.fhd_sent, log.sent), (2, 6), "MAX every boundary");
    assert_eq!(log.listed_reads, 2, "the new sender's listing is read");
    drop(log);
    assert_eq!(max.fhd_listed(), Some(FAKE_LISTED));
}

#[test]
fn switching_the_fhd_sender_off_drops_it_at_the_next_boundary() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&black(1), t0);
    worker.serve(&black(2), t0);
    max.set_fhd_enabled(false);
    worker.serve(&black(3), t0);
    let log = gpu.log();
    assert_eq!(log.drops, ["fhd sender"]);
    assert_eq!(log.targets_removed, 0, "the primary stays");
    let mut want = MAX_ALONE.to_vec();
    want.extend(FHD_BUILD);
    want.extend(BOUNDARY);
    want.extend(MAX_ALONE);
    assert_eq!(log.order, want, "FHD off: nothing is sent for it");
    assert_eq!((log.fhd_sent, log.sent), (1, 3), "MAX goes on");
    drop(log);
    let fhd = max.status().fhd;
    assert_eq!(
        (fhd.state.as_str(), fhd.reason, fhd.listed_width),
        ("off", Some(FHD_OFF_SETTING), 0)
    );
    worker.serve(&black(4), t0);
    let log = gpu.log();
    assert_eq!(log.targets_added, [MAX_TARGET], "MAX's alone");
    assert_eq!(log.fhd_senders_built, 1, "not built while off");
}

/// Switched off inside its backoff, the FHD output is built at once when it
/// is switched on again (like MAX after a release).
#[test]
fn switching_the_fhd_sender_off_forgets_its_backoff() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    gpu.script().fhd_sender.push_back(name_taken());
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&black(1), t0);
    assert_eq!(max.status().fhd.sender_backoffs, 1);
    max.set_fhd_enabled(false);
    worker.serve(&black(2), t0);
    max.set_fhd_enabled(true);
    worker.serve(&black(3), t0);
    worker.serve(&black(4), t0);
    let log = gpu.log();
    assert_eq!(
        (
            log.targets_added.len(),
            log.targets_removed,
            log.fhd_senders_built,
            log.fhd_sent
        ),
        (1, 0, 2, 1),
        "built again at once: the backoff is forgotten"
    );
}

#[test]
fn not_wanted_the_fhd_sender_is_never_built() {
    let max = MaxOut::new();
    max.set_enabled(true);
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    worker.serve(&black(1), Instant::now());
    let log = gpu.log();
    assert_eq!(log.targets_added, [MAX_TARGET], "MAX's alone");
    assert_eq!((log.fhd_senders_built, log.sent), (0, 1));
    assert_eq!(log.order, MAX_ALONE);
    assert_eq!(log.fhd_sent, 0);
    drop(log);
    let fhd = max.status().fhd;
    assert_eq!(
        (fhd.state.as_str(), fhd.submitted, fhd.failed),
        ("off", 0, 0)
    );

    // Nor while the compositor cannot be built.
    let gpu = FakeGpu::default();
    gpu.script().compositor.push_back(GpuError::NoAdapter);
    let mut worker = MaxWorker::new(&max, gpu.clone());
    worker.serve(&black(2), Instant::now());
    let log = gpu.log();
    assert!(log.targets_added.is_empty());
    assert!(log.order.is_empty());
    drop(log);
    let fhd = max.status().fhd;
    assert_eq!(
        (fhd.submitted, fhd.failed),
        (0, 0),
        "not wanted: not counted"
    );

    // Nor while MAX's boundary is not drawn (its sender refused, its
    // target taken off with it).
    let gpu = FakeGpu::default();
    gpu.script().sender.push_back(name_taken());
    let mut worker = MaxWorker::new(&max, gpu.clone());
    worker.serve(&black(3), Instant::now());
    let log = gpu.log();
    assert_eq!((log.targets_added.len(), log.targets_removed), (1, 1));
    assert_eq!(log.fhd_senders_built, 0);
    drop(log);
    let fhd = max.status().fhd;
    assert_eq!(
        (fhd.submitted, fhd.failed),
        (0, 0),
        "not wanted: not counted"
    );
}

#[test]
fn release_drops_both_senders_then_the_compositor() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    worker.serve(&black(1), Instant::now());
    worker.serve(&black(2), Instant::now());
    assert_eq!(max.fhd_listed(), Some(FAKE_LISTED), "both went out");
    worker.release(Instant::now());
    assert!(!worker.holds_gpu());
    assert_eq!(gpu.log().drops, ["sender", "fhd sender", "compositor"]);
    assert_eq!(max.fhd_listed(), None, "released: listed nowhere");
}

/// The FHD sender has its own log: its failure and its recovery, each once
/// per window, while MAX's log stays quiet.
#[test]
fn the_fhd_sender_logs_its_own_failure_and_recovery() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    let lines = worker.serve_lines(&black(1), t0, t0);
    assert_eq!(
        (lines.max, lines.fhd),
        (None, None),
        "built: nothing to say"
    );
    gpu.script().fhd_send.push_back(lost_frame());
    let lines = worker.serve_lines(&black(2), t0, t0);
    assert_eq!(lines.max, None, "MAX's boundary went out");
    assert_eq!(lines.fhd, Some(LogLine::Failing { held_back: 0 }));
    let lines = worker.serve_lines(&black(3), t0 + MS, t0 + MS);
    assert_eq!(lines.fhd, None, "recovered inside the window: held back");
    let later = t0 + Duration::from_secs(5);
    let lines = worker.serve_lines(&black(4), later, later);
    assert_eq!(lines.fhd, Some(LogLine::Recovered { held_back: 1 }));
    assert_eq!(lines.max, None);
    assert_eq!(max.status().fhd.failed, 1, "one lost frame, no backoff");
}

/// A registry that cannot be read yet is read again at the next boundary,
/// and never again once it answered.
#[test]
fn an_unreadable_listing_is_read_again_until_it_answers() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    gpu.script().listed.push_back(None);
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&black(1), t0);
    assert_eq!(gpu.log().listed_reads, 0, "not sent yet: not read");
    worker.serve(&black(2), t0);
    assert_eq!((max.fhd_listed(), gpu.log().listed_reads), (None, 1));
    worker.serve(&black(3), t0);
    assert_eq!(
        (max.fhd_listed(), gpu.log().listed_reads),
        (Some(FAKE_LISTED), 2)
    );
    worker.serve(&black(4), t0);
    assert_eq!(gpu.log().listed_reads, 2, "not read again");
}

#[test]
fn the_loop_releases_both_while_max_is_off() {
    let max = Arc::new(fhd_on());
    let gpu = FakeGpu::default();
    let thread = spawn_loop(&max, &gpu);
    wait_until("the loop takes jobs", || max.accepting());
    max.offer_with(|| black(1));
    wait_until("MAX went out, the FHD output built", || {
        gpu.log().fhd_senders_built == 1
    });
    max.offer_with(|| black(2));
    wait_until("both went out", || max.status().fhd.submitted == 1);
    assert_eq!(max.status().submitted, 2);
    max.set_enabled(false);
    wait_until("off releases both", || gpu.log().drops.len() == 3);
    assert_eq!(gpu.log().drops, ["sender", "fhd sender", "compositor"]);
    let fhd = max.status().fhd;
    assert_eq!(
        (fhd.state.as_str(), fhd.reason),
        ("off", Some("max_off")),
        "MAX off ⇒ FHD off"
    );
    max.stop();
    wait_until("the loop exits on stop", || thread.is_finished());
    thread.join().expect("the loop");
}

/// #243 follow-up: a quiesce drops both senders, then the compositor,
/// holds nothing more and reports `quiesced` for both.
#[test]
fn a_quiesce_drops_both_senders_and_reports_quiesced() {
    let max = fhd_on();
    let _consumer = max.attach();
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let a = picture(4, 2);
    worker.serve(&plain(1, &a), Instant::now());
    worker.serve(&plain(2, &a), Instant::now());
    assert_eq!(max.status().fhd.submitted, 1, "both went out");
    worker.quiesce();
    assert!(!worker.holds_gpu());
    assert_eq!(gpu.log().drops, ["sender", "fhd sender", "compositor"]);
    let status = max.status();
    assert_eq!(
        (status.state.as_str(), status.fhd.state.as_str()),
        ("quiesced", "quiesced")
    );
    assert_eq!(status.fhd.listed_width, 0, "no listing kept");
}

/// #243 follow-up: the quiesce expired with the process alive: the resume
/// reads `running` again for both senders before any boundary, and the
/// next jobs build the compositor, MAX's target and both senders anew.
#[test]
fn a_resume_reads_running_and_the_next_job_builds_both_senders_anew() {
    let max = fhd_on();
    let _consumer = max.attach();
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let a = picture(4, 2);
    worker.serve(&plain(1, &a), Instant::now());
    worker.serve(&plain(2, &a), Instant::now());
    worker.quiesce();
    worker.resume();
    let status = max.status();
    assert_eq!(
        (status.state.as_str(), status.fhd.state.as_str()),
        ("running", "running"),
        "resumed: no longer quiesced"
    );
    worker.serve(&plain(3, &a), Instant::now());
    worker.serve(&plain(4, &a), Instant::now());
    let status = max.status();
    assert_eq!((status.submitted, status.fhd.submitted), (4, 2));
    let log = gpu.log();
    assert_eq!(
        (
            log.compositors_built,
            log.senders_built,
            log.targets_added.len(),
            log.fhd_senders_built
        ),
        (2, 2, 2, 2),
        "built anew"
    );
}

/// #251 (19.10.2026): both senders hand their frame to Spout the way
/// `program_max_spout_send` says at that boundary, given to each before its
/// send (the FHD sender from the boundary after its build).
#[test]
fn every_send_takes_the_spout_hand_off_set_at_its_boundary() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&black(1), t0);
    assert!(max.set_spout_send(SpoutSend::Plain), "a change");
    assert!(!max.set_spout_send(SpoutSend::Plain), "the same again");
    worker.serve(&black(2), t0);
    assert!(max.set_spout_send(SpoutSend::ReleaseThenWait));
    worker.serve(&black(3), t0);
    assert_eq!(
        gpu.log().send_modes,
        [
            ("max", SpoutSend::Synced),
            ("max", SpoutSend::Plain),
            ("fhd", SpoutSend::Plain),
            ("max", SpoutSend::ReleaseThenWait),
            ("fhd", SpoutSend::ReleaseThenWait),
        ]
    );
}
