//! #239: the `SP-program` Spout sender on the `program-max` thread, on the
//! fake GPU: built on the thread only while it is wanted, the same pictures
//! as MAX at the same weight, built, composed and sent only after MAX's
//! paced send (never delaying it), its own failures, backoff, lost device,
//! telemetry and log, never MAX's, and dropped when switched off.
//! Wired via `#[cfg(test)] #[path = "program_max_worker_tests_fhd.rs"] mod tests_fhd;`.

use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use sp_gpu::GpuError;

use super::tests::{
    DRAW_US, Drawn, FAKE_LISTED, FHD_DRAW_US, FHD_SEND_US, FakeGpu, Gate, Hold, SEND_US, UPLOAD_US,
    device_lost, not_registered, pic, picture, plain, spawn_loop, wait_until,
};
use super::{LogLine, MAX_RETRY_BACKOFF, MaxWorker};
use crate::playback::program_max::{FHD_OFF_SETTING, MaxJob, MaxOut};
use crate::playback::program_max_send::tests::{FakeClock, waits};

const MS: Duration = Duration::from_millis(1);

/// MAX on and the FHD sender's setting on.
fn fhd_on() -> MaxOut {
    let max = MaxOut::new();
    max.set_enabled(true);
    max.set_fhd_enabled(true);
    max
}

fn black(stamp: i64) -> MaxJob {
    MaxJob::Black { stamp_100ns: stamp }
}

#[test]
fn the_fhd_sender_draws_the_same_boundaries_as_max_and_sends_after_it() {
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
    assert_eq!(
        (log.fhd_compositors_built, log.fhd_senders_built),
        (1, 1),
        "built once, on the thread"
    );
    let drawn = [
        Drawn::Picture(pic(1, &a)),
        Drawn::Fade(Some(pic(1, &a)), Some(pic(2, &b)), 64),
        Drawn::Black,
    ];
    assert_eq!(log.drawn, drawn, "MAX");
    assert_eq!(log.fhd_drawn, drawn, "the same pictures, ids and weight");
    let job = ["max draw", "max send", "fhd draw", "fhd send"];
    let mut want = vec!["max draw", "max send", "fhd build"];
    want.extend(&job[2..]);
    want.extend(job.iter().chain(&job));
    assert_eq!(log.order, want, "MAX whole first, then the FHD sender");
    assert_eq!(log.listed_reads, 1, "the registry is read once");
    drop(log);
    let status = max.status();
    assert_eq!((status.submitted, status.draw_us_p99), (3, DRAW_US));
    assert_eq!(status.send_us_p99, SEND_US);
    let fhd = status.fhd;
    assert_eq!(
        (fhd.state.as_str(), fhd.reason, fhd.submitted, fhd.failed),
        ("running", None, 3, 0)
    );
    assert_eq!(
        (fhd.upload_us_p99, fhd.draw_us_p99, fhd.send_us_p99),
        (UPLOAD_US, FHD_DRAW_US, FHD_SEND_US)
    );
    assert_eq!((fhd.listed_width, fhd.listed_height), FAKE_LISTED);
}

/// MAX goes first, whole: composed, then sent at its due instant; the FHD
/// sender is built and composed only after MAX's send, and goes out at once
/// after it (its own send waits for nothing: the instant has passed).
#[test]
fn max_is_sent_at_its_due_instant_before_the_fhd_sender_is_even_composed() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    let offered = Instant::now();
    let (mut clock, list) = FakeClock::at(offered + 4 * MS);
    let at_wait = Arc::new(Mutex::new(Vec::new()));
    let (seen, gpu_then) = (at_wait.clone(), gpu.clone());
    clock.on_wait = Some(Box::new(move || {
        let order = gpu_then.log().order.clone();
        seen.lock().unwrap_or_else(|p| p.into_inner()).push(order);
    }));
    let mut worker = MaxWorker::new(&max, gpu.clone()).with_clock(Box::new(clock));
    worker.serve_offered(&black(1), offered, offered + 4 * MS);

    let due = offered + 12 * MS;
    assert_eq!(
        *at_wait.lock().unwrap_or_else(|p| p.into_inner()),
        [
            vec!["max draw"],
            vec!["max draw", "max send", "fhd build", "fhd draw"]
        ],
        "MAX's wait sees only MAX's draw; the FHD sender's (a no-op) comes after"
    );
    assert_eq!(waits(&list), [due, due], "no FHD wait past MAX's instant");
    assert_eq!(
        gpu.log().order,
        ["max draw", "max send", "fhd build", "fhd draw", "fhd send"]
    );
    let status = max.status();
    assert_eq!(
        (status.send_at_us_max, status.send_late),
        (12_000, 0),
        "MAX at its due instant, on time"
    );
}

/// A slow FHD compose (held behind a gate, on the real loop) never holds
/// MAX back: MAX's boundary is already out while the FHD compose waits.
#[test]
fn a_slow_fhd_compose_never_holds_max_back() {
    let max = Arc::new(fhd_on());
    let gpu = FakeGpu::default();
    let gate = Arc::new(Gate::default());
    let (entered, fhd_entered) = mpsc::channel();
    gpu.hold_next_fhd_compose(Hold {
        entered,
        gate: gate.clone(),
    });
    let thread = spawn_loop(&max, &gpu);
    wait_until("the loop takes jobs", || max.accepting());
    max.offer_with(|| black(1));
    let held = fhd_entered.recv_timeout(Duration::from_secs(20));
    let order = gpu.log().order.clone();
    gate.open();
    held.expect("the FHD compose was entered");
    assert_eq!(
        order,
        ["max draw", "max send", "fhd build"],
        "MAX sent while the FHD compose is still held"
    );
    wait_until("the FHD boundary went out", || {
        max.status().fhd.submitted == 1
    });
    assert_eq!(max.status().submitted, 1);
    max.stop();
    wait_until("the loop exits on stop", || thread.is_finished());
    thread.join().expect("the loop");
}

/// MAX's boundary failing does not hold the FHD sender back: it goes out
/// on its own, at the due instant it paces itself.
#[test]
fn with_max_failing_the_fhd_sender_still_goes_out_at_the_due_instant() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    gpu.script().compositor.push_back(GpuError::NoAdapter);
    let offered = Instant::now();
    let (clock, list) = FakeClock::at(offered + 4 * MS);
    let mut worker = MaxWorker::new(&max, gpu.clone()).with_clock(Box::new(clock));
    worker.serve_offered(&black(1), offered, offered + 4 * MS);

    assert_eq!(waits(&list), [offered + 12 * MS]);
    assert_eq!(gpu.log().order, ["fhd build", "fhd draw", "fhd send"]);
    let status = max.status();
    assert_eq!(
        (status.state, status.submitted, status.failed),
        (format!("error: {}", GpuError::NoAdapter), 0, 1)
    );
    assert_eq!(
        (status.fhd.state.as_str(), status.fhd.submitted),
        ("running", 1)
    );
    assert!(worker.holds_gpu(), "the FHD sender's objects alone");
    worker.release();
    assert!(!worker.holds_gpu());
    assert_eq!(gpu.log().drops, ["fhd sender", "fhd compositor"]);
}

#[test]
fn a_failed_fhd_build_waits_its_own_backoff_and_never_stops_max() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    gpu.script().fhd_compositor.push_back(GpuError::NoAdapter);
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&black(1), t0);
    let fhd = max.status().fhd;
    assert_eq!(fhd.state, format!("error: {}", GpuError::NoAdapter));
    assert_eq!((fhd.failed, fhd.submitted), (1, 0));
    worker.serve(&black(2), t0 + MAX_RETRY_BACKOFF - MS);
    assert_eq!(gpu.log().fhd_compositors_built, 1, "inside its backoff");
    assert_eq!(max.status().fhd.failed, 2, "a skipped boundary counts");
    worker.serve(&black(3), t0 + MAX_RETRY_BACKOFF);
    let log = gpu.log();
    assert_eq!((log.fhd_compositors_built, log.fhd_sent), (2, 1));
    assert_eq!(log.sent, 3, "MAX went out every time");
    drop(log);
    let status = max.status();
    assert_eq!((status.state.as_str(), status.failed), ("running", 0));
    assert_eq!(status.fhd.state, "running");
}

#[test]
fn a_refused_fhd_sender_is_dropped_and_a_new_one_waits_its_own_backoff() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&black(1), t0);
    assert_eq!(max.fhd_listed(), Some(FAKE_LISTED));
    gpu.script().fhd_send.push_back(not_registered());
    worker.serve(&black(2), t0);
    assert_eq!(gpu.log().drops, ["fhd sender"], "its compositor stays");
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
    worker.serve(&black(3), t0 + MAX_RETRY_BACKOFF - MS);
    assert_eq!(gpu.log().fhd_senders_built, 1, "inside its backoff");
    worker.serve(&black(4), t0 + MAX_RETRY_BACKOFF);
    let log = gpu.log();
    assert_eq!(
        (log.fhd_compositors_built, log.fhd_senders_built),
        (1, 2),
        "a new sender on the same compositor"
    );
    assert_eq!((log.fhd_sent, log.sent), (2, 4));
    assert_eq!(log.listed_reads, 2, "the new sender's listing is read");
    drop(log);
    assert_eq!(max.fhd_listed(), Some(FAKE_LISTED));
}

#[test]
fn a_lost_fhd_device_drops_only_the_fhd_objects() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    gpu.script().fhd_compose.push_back(device_lost());
    gpu.script().fhd_compose.push_back(device_lost());
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&black(1), t0);
    assert_eq!(gpu.log().drops, ["fhd sender", "fhd compositor"]);
    assert_eq!(max.status().device_resets, 0, "MAX's device is fine");
    worker.serve(&black(2), t0);
    assert_eq!(
        gpu.log().fhd_compositors_built,
        2,
        "rebuilt at once, and lost again before an FHD boundary went out"
    );
    worker.serve(&black(3), t0);
    assert_eq!(
        gpu.log().fhd_compositors_built,
        2,
        "now it waits the backoff"
    );
    worker.serve(&black(4), t0 + MAX_RETRY_BACKOFF);
    let log = gpu.log();
    assert_eq!((log.fhd_compositors_built, log.fhd_sent), (3, 1));
    assert_eq!(
        (log.compositors_built, log.sent),
        (1, 4),
        "MAX kept its objects and every boundary"
    );
}

/// An FHD boundary that went out between two losses: the second rebuild
/// does not wait (its own lost-device state, like MAX's).
#[test]
fn a_lost_fhd_device_after_a_sent_fhd_boundary_rebuilds_at_once_again() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    gpu.script().fhd_send.push_back(device_lost());
    worker.serve(&black(1), t0);
    worker.serve(&black(2), t0);
    assert_eq!(gpu.log().fhd_sent, 1, "rebuilt and sent");
    gpu.script().fhd_send.push_back(device_lost());
    worker.serve(&black(3), t0);
    worker.serve(&black(4), t0);
    let log = gpu.log();
    assert_eq!(
        (log.fhd_compositors_built, log.fhd_sent),
        (3, 2),
        "an FHD boundary went out between the two losses: no backoff"
    );
    assert_eq!(log.sent, 4);
}

#[test]
fn switching_the_fhd_sender_off_drops_it_at_the_next_boundary() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&black(1), t0);
    max.set_fhd_enabled(false);
    worker.serve(&black(2), t0);
    let log = gpu.log();
    assert_eq!(log.drops, ["fhd sender", "fhd compositor"]);
    assert_eq!((log.fhd_sent, log.sent), (1, 2), "MAX goes on");
    drop(log);
    let fhd = max.status().fhd;
    assert_eq!(
        (fhd.state.as_str(), fhd.reason, fhd.listed_width),
        ("off", Some(FHD_OFF_SETTING), 0)
    );
    worker.serve(&black(3), t0);
    assert_eq!(gpu.log().fhd_compositors_built, 1, "not built while off");
}

/// Switched off inside its backoff, the FHD sender is built at once when it
/// is switched on again (like MAX after a release).
#[test]
fn switching_the_fhd_sender_off_forgets_its_backoff() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    gpu.script().fhd_sender.push_back(GpuError::SpoutNameTaken {
        name: "SP-program".into(),
    });
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&black(1), t0);
    assert_eq!(max.status().fhd.sender_backoffs, 1);
    max.set_fhd_enabled(false);
    worker.serve(&black(2), t0);
    max.set_fhd_enabled(true);
    worker.serve(&black(3), t0);
    let log = gpu.log();
    assert_eq!(
        (
            log.fhd_compositors_built,
            log.fhd_senders_built,
            log.fhd_sent
        ),
        (2, 2, 1),
        "rebuilt at once: the backoff is forgotten"
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
    assert_eq!((log.fhd_compositors_built, log.sent), (0, 1));
    assert_eq!(log.order, ["max draw", "max send"]);
    drop(log);
    let fhd = max.status().fhd;
    assert_eq!((fhd.state.as_str(), fhd.submitted), ("off", 0));

    // Nor while MAX cannot be built (when the FHD side would be built first).
    let gpu = FakeGpu::default();
    gpu.script().compositor.push_back(GpuError::NoAdapter);
    let mut worker = MaxWorker::new(&max, gpu.clone());
    worker.serve(&black(2), Instant::now());
    let log = gpu.log();
    assert_eq!((log.fhd_compositors_built, log.order.len()), (0, 0));
    drop(log);
    assert_eq!(max.status().fhd.submitted, 0);
}

#[test]
fn release_drops_max_then_the_fhd_sender_each_sender_first() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    worker.serve(&black(1), Instant::now());
    worker.release();
    assert!(!worker.holds_gpu());
    assert_eq!(
        gpu.log().drops,
        ["sender", "compositor", "fhd sender", "fhd compositor"]
    );
    assert_eq!(max.fhd_listed(), None, "released: listed nowhere");
}

/// The FHD sender has its own log: its failure and its recovery, each once
/// per window, while MAX's log stays quiet.
#[test]
fn the_fhd_sender_logs_its_own_failure_and_recovery() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    gpu.script().fhd_send.push_back(GpuError::Spout {
        call: "spout_sender_send",
        code: 3,
    });
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    let lines = worker.serve_lines(&black(1), t0, t0);
    assert_eq!(lines.max, None, "MAX's boundary went out");
    assert_eq!(lines.fhd, Some(LogLine::Failing { held_back: 0 }));
    let lines = worker.serve_lines(&black(2), t0 + MS, t0 + MS);
    assert_eq!(lines.fhd, None, "recovered inside the window: held back");
    let later = t0 + Duration::from_secs(5);
    let lines = worker.serve_lines(&black(3), later, later);
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
    assert_eq!((max.fhd_listed(), gpu.log().listed_reads), (None, 1));
    worker.serve(&black(2), t0);
    assert_eq!(
        (max.fhd_listed(), gpu.log().listed_reads),
        (Some(FAKE_LISTED), 2)
    );
    worker.serve(&black(3), t0);
    assert_eq!(gpu.log().listed_reads, 2, "not read again");
}

#[test]
fn the_loop_releases_both_while_max_is_off() {
    let max = Arc::new(fhd_on());
    let gpu = FakeGpu::default();
    let thread = spawn_loop(&max, &gpu);
    wait_until("the loop takes jobs", || max.accepting());
    max.offer_with(|| black(1));
    wait_until("both went out", || max.status().fhd.submitted == 1);
    assert_eq!(max.status().submitted, 1);
    max.set_enabled(false);
    wait_until("off releases both", || gpu.log().drops.len() == 4);
    assert_eq!(
        gpu.log().drops,
        ["sender", "compositor", "fhd sender", "fhd compositor"]
    );
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

/// Off Windows the production GPU's FHD compositor has no Direct3D either.
#[cfg(not(windows))]
#[test]
fn off_windows_the_production_fhd_compositor_is_unsupported() {
    use super::{MaxGpu, SpoutGpu};
    assert!(matches!(
        SpoutGpu.fhd_compositor(),
        Err(GpuError::Unsupported)
    ));
}
