//! #239 + #251 step 2: a lost device with the `SP-program` Spout sender on
//! (MAX's and the FHD output share ONE compositor, so a lost device is one
//! event for both), on the fake GPU: wherever it is lost (a build, the
//! compose, a send, the FHD output's build before or after MAX's send) both
//! outputs fail with it, one reset, both rebuilt; a loss after MAX's
//! boundary went out leaves that boundary sent and MAX's state and log name
//! it; a loss while building, or a second loss before a boundary of either
//! output went out, waits the backoff. Split from
//! `program_max_worker_tests_fhd.rs` (the 1000-line cap).
//! Wired via `#[cfg(test)] #[path = "program_max_worker_tests_fhd_lost.rs"] mod tests_fhd_lost;`.

use std::time::Instant;

use sp_gpu::MAX_TARGET;

use super::tests::{FakeGpu, device_lost};
use super::tests_fhd::{black, fhd_on, name_taken};
use super::{LogLine, MAX_RETRY_BACKOFF, MaxWorker};

/// A lost device while MAX's sender is built is the device's: the FHD
/// output fails with that very error, builds no sender, and both wait.
#[test]
fn a_lost_device_while_building_max_s_sender_fails_the_fhd_output_too() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    gpu.script().sender.push_back(device_lost());
    let mut worker = MaxWorker::new(&max, gpu.clone());
    worker.serve(&black(1), Instant::now());
    let log = gpu.log();
    assert_eq!(log.drops, ["compositor"]);
    assert_eq!(log.targets_added, [MAX_TARGET], "MAX's, before its sender");
    assert_eq!(log.fhd_senders_built, 0, "no FHD sender on a lost device");
    drop(log);
    let status = max.status();
    let why = format!("error: {}", device_lost());
    assert_eq!(
        (status.device_resets, status.state.as_str()),
        (1, why.as_str())
    );
    assert_eq!(status.fhd.state, why, "the same error on the FHD output");
}

/// With MAX's sender waiting out its backoff the FHD output is built first;
/// a device lost there is the device's: MAX names it too, nothing is drawn,
/// one reset, and both wait the backoff.
#[test]
fn a_lost_device_while_building_the_fhd_output_first_fails_max_too() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    gpu.script().sender.push_back(name_taken());
    gpu.script().fhd_sender.push_back(device_lost());
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&black(1), t0);
    let log = gpu.log();
    assert_eq!(log.drops, ["compositor"]);
    assert!(
        log.drawn.is_empty() && log.fhd_drawn.is_empty(),
        "nothing drawn on a lost device"
    );
    drop(log);
    let status = max.status();
    let why = format!("error: {}", device_lost());
    assert_eq!(
        (
            status.state.as_str(),
            status.device_resets,
            status.sender_backoffs
        ),
        (why.as_str(), 1, 1),
        "MAX names the device, never its own refusal alone"
    );
    assert_eq!(status.fhd.state, why);
    worker.serve(&black(2), t0);
    assert_eq!(
        gpu.log().compositors_built,
        1,
        "lost while building: the backoff"
    );
}

/// A device lost while the FHD output is built after MAX's send: MAX's
/// boundary had gone out; the FHD output fails with the loss, one reset,
/// and the rebuild waits the backoff (a loss while building). MAX's device
/// is gone too: its boundary stays sent, never failed, and its state and
/// log name the loss through the backoff (never `running` while dark).
#[test]
fn a_lost_device_while_building_the_fhd_output_after_max_s_send_waits_the_backoff() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    gpu.script().fhd_sender.push_back(device_lost());
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    let lines = worker.serve_lines(&black(1), t0, t0);
    assert_eq!(
        lines.max,
        Some(LogLine::Failing { held_back: 0 }),
        "MAX's log names it"
    );
    assert_eq!(lines.fhd, Some(LogLine::Failing { held_back: 0 }));
    let log = gpu.log();
    assert_eq!(log.sent, 1, "MAX's boundary went out before the build");
    assert_eq!(log.drops, ["sender", "compositor"]);
    drop(log);
    let status = max.status();
    let why = format!("error: {}", device_lost());
    assert_eq!(
        (status.submitted, status.failed, status.device_resets),
        (1, 0, 1)
    );
    assert_eq!(
        (status.state.as_str(), status.fhd.state.as_str()),
        (why.as_str(), why.as_str())
    );
    worker.serve(&black(2), t0);
    assert_eq!(gpu.log().compositors_built, 1, "inside the backoff");
    let status = max.status();
    assert_eq!((status.failed, status.fhd.failed), (1, 2), "both skipped");
    assert_eq!(status.state, why, "MAX still names the loss");
}

/// A lost device at the FHD send is ONE event for both outputs (one
/// device): both senders and the compositor dropped, one reset, both
/// rebuilt on the next job.
#[test]
fn a_lost_device_drops_both_senders_and_the_compositor_and_rebuilds_both() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&black(1), t0);
    gpu.script().fhd_send.push_back(device_lost());
    worker.serve(&black(2), t0);
    assert!(!worker.holds_gpu());
    assert_eq!(gpu.log().drops, ["sender", "fhd sender", "compositor"]);
    let status = max.status();
    assert_eq!(
        (status.device_resets, status.submitted, status.failed),
        (1, 2, 0)
    );
    let why = format!("error: {}", device_lost());
    assert_eq!(
        (status.state.as_str(), status.fhd.state.as_str()),
        (why.as_str(), why.as_str())
    );
    worker.serve(&black(3), t0);
    assert_eq!(
        max.status().state,
        "running",
        "MAX sent on the rebuilt device"
    );
    let log = gpu.log();
    assert_eq!(
        (
            log.compositors_built,
            log.senders_built,
            log.targets_added.len(),
            log.fhd_senders_built
        ),
        (2, 2, 2, 2),
        "both rebuilt at once (MAX's target with its sender)"
    );
    drop(log);
    worker.serve(&black(4), t0);
    let log = gpu.log();
    assert_eq!((log.sent, log.fhd_sent), (4, 1));
    drop(log);
    assert_eq!(max.status().fhd.state, "running");
}

/// MAX's boundary went out BEFORE the FHD send lost the device, so it
/// proves nothing about the rebuilt one: lost again at the next compose
/// (no boundary since), the rebuild waits the backoff.
#[test]
fn a_boundary_sent_before_the_loss_does_not_count_as_one_after_it() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&black(1), t0);
    gpu.script().fhd_send.push_back(device_lost());
    worker.serve(&black(2), t0);
    assert_eq!(gpu.log().sent, 2, "MAX went out, then the FHD send lost it");
    gpu.script().compose.push_back(device_lost());
    worker.serve(&black(3), t0);
    assert_eq!(
        gpu.log().compositors_built,
        2,
        "the first loss rebuilds at once"
    );
    worker.serve(&black(4), t0);
    assert_eq!(
        gpu.log().compositors_built,
        2,
        "lost again before a boundary went out on the rebuilt device: the backoff"
    );
    assert_eq!(max.status().device_resets, 2);
}

/// A lost device at MAX's send fails the FHD output with it (its sender is
/// gone with the device): nothing more is sent, one reset.
#[test]
fn a_lost_device_at_max_s_send_fails_the_fhd_output_with_it() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    worker.serve(&black(1), Instant::now());
    gpu.script().send.push_back(device_lost());
    worker.serve(&black(2), Instant::now());
    let log = gpu.log();
    assert_eq!((log.sent, log.fhd_sent), (1, 0));
    assert_eq!(log.drops, ["sender", "fhd sender", "compositor"]);
    drop(log);
    let status = max.status();
    let why = format!("error: {}", device_lost());
    assert_eq!(
        (status.device_resets, status.state.as_str()),
        (1, why.as_str())
    );
    assert_eq!(
        (status.fhd.state.as_str(), status.fhd.failed),
        (why.as_str(), 1)
    );
}

/// A lost device at the compose fails both, and a second loss before a
/// boundary of either output went out waits the backoff for both.
#[test]
fn a_second_lost_device_at_the_compose_waits_the_backoff_for_both() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    gpu.script().compose.push_back(device_lost());
    gpu.script().compose.push_back(device_lost());
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&black(1), t0);
    worker.serve(&black(2), t0);
    assert_eq!(
        gpu.log().compositors_built,
        2,
        "rebuilt at once, lost again"
    );
    worker.serve(&black(3), t0);
    assert_eq!(gpu.log().compositors_built, 2, "now it waits the backoff");
    let status = max.status();
    assert_eq!(
        (status.device_resets, status.failed, status.fhd.failed),
        (2, 3, 3),
        "one reset per loss; both outputs fail and skip"
    );
    worker.serve(&black(4), t0 + MAX_RETRY_BACKOFF);
    worker.serve(&black(5), t0 + MAX_RETRY_BACKOFF);
    let log = gpu.log();
    assert_eq!((log.compositors_built, log.sent, log.fhd_sent), (3, 2, 1));
}

/// A boundary of EITHER output proves the rebuilt device: with MAX's sender
/// in its backoff, the FHD boundary alone lets the next loss rebuild at
/// once.
#[test]
fn an_fhd_boundary_alone_proves_the_rebuilt_device() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    gpu.script().compose.push_back(device_lost());
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&black(1), t0);
    gpu.script().sender.push_back(name_taken());
    worker.serve(&black(2), t0);
    assert_eq!(gpu.log().fhd_sent, 1, "the FHD target went out alone");
    gpu.script().compose.push_back(device_lost());
    worker.serve(&black(3), t0);
    worker.serve(&black(4), t0);
    let log = gpu.log();
    assert_eq!(
        (log.compositors_built, log.fhd_sent),
        (3, 2),
        "an FHD boundary went out between the two losses: the rebuild does not wait"
    );
}
