//! #279 P1b: the `program-max` thread reads NDI `SP-program`'s picture
//! back, on the fake GPU: the UYVY pass on the compositor's primary (FHD)
//! target while NDI comes from the GPU, its readback pushed into the feed
//! after the compose and BEFORE MAX's send, a Black boundary's cached black
//! (nothing mapped), MAX off composing the FHD target alone, the pass off
//! with NDI, a failed enable's backoff, a device lost at the enable, the
//! pass's log (a release says off), and what a failed readback, a lost
//! device or a coalesced job leave in the feed (nothing: the `SP-program`
//! sender repeats). The thread never lets the feed's pictures go: the
//! sender does (review round 5).
//! Wired via `#[cfg(test)] #[path = "program_max_worker_tests_ndi.rs"] mod tests_ndi;`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use sp_core::config::NdiSource;
use sp_gpu::{GpuError, MAX_TARGET};

use super::tests::{
    Drawn, FakeGpu, UYVY_US, device_lost, fake_uyvy, not_registered, pic, picture, plain,
};
use super::{LogLine, MAX_LOG_EVERY_100NS, MAX_RETRY_BACKOFF, MaxWorker, PassLine};
use crate::playback::ndi_feed::{UYVY_BLACK_PAIR, uyvy_len};
use crate::playback::program_max::{MaxJob, MaxOut};

/// MAX on or off, NDI from the GPU.
fn ndi_on(max_enabled: bool) -> MaxOut {
    let max = MaxOut::new();
    max.set_enabled(max_enabled);
    max.set_ndi_source(NdiSource::Gpu);
    max
}

fn black(stamp: i64) -> MaxJob {
    MaxJob::Black { stamp_100ns: stamp }
}

/// The feed's picture of `stamp`, as bytes (`None`: none came).
fn taken(max: &MaxOut, stamp: i64) -> Option<Vec<u8>> {
    max.ndi.take(stamp).map(|picture| picture.to_vec())
}

/// A failed GPU call of the UYVY pass (its enable or its readback).
fn gpu_refusal() -> GpuError {
    GpuError::NoObject {
        call: "the test's UYVY pass",
    }
}

#[test]
fn ndi_s_readback_is_pushed_after_the_compose_and_before_max_s_send() {
    let max = ndi_on(true);
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let a = picture(4, 2);
    let t0 = Instant::now();
    worker.serve(&plain(1, &a), t0);
    assert_eq!(
        gpu.log().order,
        ["ndi on", "fhd draw", "max draw", "ndi read", "max send"],
        "the pass on the primary, the readback between the compose and the send"
    );
    assert_eq!(max.ndi.newest_stamp(), Some(1));
    worker.serve(&plain(2, &a), t0);
    assert_eq!(taken(&max, 1), Some(fake_uyvy(1)), "the first compose's");
    assert_eq!(taken(&max, 2), Some(fake_uyvy(2)), "the second's");
    let log = gpu.log();
    assert_eq!(
        log.uyvy_enabled, 1,
        "enabled once, for the compositor's life"
    );
    assert_eq!(log.uyvy_read, 2);
    assert_eq!(log.sent, 2);
    drop(log);
    let status = max.ndi_status();
    assert_eq!(
        status.uyvy_us_p99, UYVY_US,
        "the pass's part of the compose"
    );
    assert_eq!(max.status().submitted, 2, "MAX unchanged");
}

#[test]
fn a_black_boundary_pushes_the_cached_black_and_maps_nothing() {
    let max = ndi_on(false);
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&black(1), t0);
    worker.serve(&black(2), t0);
    assert_eq!(gpu.log().uyvy_read, 0, "nothing mapped");
    assert_eq!(gpu.log().order, ["ndi on", "fhd draw", "fhd draw"]);
    let first = max.ndi.take(1).expect("the black of boundary 1");
    let second = max.ndi.take(2).expect("the black of boundary 2");
    assert_eq!(first.len(), uyvy_len(1920, 1080));
    assert_eq!(first.len(), 4_147_200);
    assert!(
        first.chunks(4).all(|pair| pair == UYVY_BLACK_PAIR),
        "limited-range black pairs"
    );
    assert!(second.ptr_eq(&first), "built once, the same allocation");
}

#[test]
fn max_off_with_ndi_on_composes_the_fhd_target_alone() {
    let max = ndi_on(false);
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let a = picture(4, 2);
    let t0 = Instant::now();
    worker.serve(&plain(1, &a), t0);
    let log = gpu.log();
    assert_eq!(log.order, ["ndi on", "fhd draw", "ndi read"]);
    assert_eq!((log.compositors_built, log.senders_built), (1, 0));
    assert!(log.targets_added.is_empty(), "no MAX target");
    assert!(log.drawn.is_empty(), "nothing drawn for MAX");
    assert_eq!(log.fhd_drawn, [Drawn::Picture(pic(1, &a))]);
    drop(log);
    assert_eq!(max.ndi.newest_stamp(), Some(1));
    let status = max.status();
    assert_eq!(
        (status.state.as_str(), status.submitted, status.failed),
        ("off", 0, 0),
        "MAX reads off, nothing counted for it"
    );

    // MAX on: its target and sender are built, both targets drawn.
    max.set_enabled(true);
    worker.serve(&plain(2, &a), t0);
    let log = gpu.log();
    assert_eq!(log.targets_added, [MAX_TARGET]);
    assert_eq!(
        log.order[3..],
        ["fhd draw", "max draw", "ndi read", "max send"]
    );
    drop(log);

    // MAX off again: its sender, then its target go; the FHD target stays.
    max.set_enabled(false);
    worker.serve(&plain(3, &a), t0);
    let log = gpu.log();
    assert_eq!(log.drops, ["sender"], "the compositor stays");
    assert_eq!(log.targets_removed, 1);
    assert_eq!(
        log.order[7..],
        ["max target removed", "fhd draw", "ndi read"]
    );
    assert_eq!(log.sent, 1, "MAX sent boundary 2 only");
    drop(log);
    assert_eq!(max.ndi.newest_stamp(), Some(3));
    assert!(worker.holds_gpu());
}

/// MAX switched off while its sender waits a backoff (NDI keeps the
/// compositor): the backoff goes with MAX's output, so MAX on again builds
/// at once.
#[test]
fn switching_max_off_with_ndi_on_forgets_max_s_backoff() {
    let max = ndi_on(true);
    let gpu = FakeGpu::default();
    gpu.script().sender.push_back(not_registered());
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let a = picture(4, 2);
    let t0 = Instant::now();
    worker.serve(&plain(1, &a), t0);
    let log = gpu.log();
    assert_eq!(
        (log.senders_built, log.targets_removed, log.sent),
        (1, 1, 0),
        "refused: MAX's sender and target dropped, its backoff runs"
    );
    drop(log);
    max.set_enabled(false);
    worker.serve(&plain(2, &a), t0);
    max.set_enabled(true);
    worker.serve(&plain(3, &a), t0);
    let log = gpu.log();
    assert_eq!(
        (log.senders_built, log.sent),
        (2, 1),
        "built at once: the backoff went with MAX's output"
    );
    drop(log);
    assert_eq!(max.ndi.newest_stamp(), Some(3), "NDI read every boundary");
}

#[test]
fn ndi_back_on_the_canvas_disables_the_pass() {
    let max = ndi_on(true);
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let a = picture(4, 2);
    let t0 = Instant::now();
    worker.serve(&plain(1, &a), t0);
    max.set_ndi_source(NdiSource::Cpu);
    worker.serve(&plain(2, &a), t0);
    max.set_ndi_source(NdiSource::Gpu);
    worker.serve(&plain(3, &a), t0);
    let log = gpu.log();
    assert_eq!(
        log.order,
        [
            "ndi on", "fhd draw", "max draw", "ndi read", "max send", // NDI on
            "ndi off", "fhd draw", "max draw", "max send", // off: no readback
            "ndi on", "fhd draw", "max draw", "ndi read", "max send", // on again
        ]
    );
    assert_eq!(log.uyvy_enabled, 2);
    drop(log);
    assert_eq!(
        taken(&max, 1),
        Some(fake_uyvy(1)),
        "the pass went off, the picture stays: the SP-program sender lets it go (review round 5)"
    );
    assert_eq!(taken(&max, 2), None, "boundary 2 was not read back");
    assert_eq!(taken(&max, 3), Some(fake_uyvy(3)));
}

#[test]
fn a_failed_enable_is_tried_again_after_the_backoff() {
    let max = ndi_on(true);
    let gpu = FakeGpu::default();
    gpu.script().enable_uyvy.push_back(gpu_refusal());
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let a = picture(4, 2);
    let t0 = Instant::now();
    worker.serve(&plain(1, &a), t0);
    worker.serve(
        &plain(2, &a),
        t0 + MAX_RETRY_BACKOFF - Duration::from_millis(1),
    );
    assert_eq!(
        gpu.log().order,
        [
            "ndi on", "fhd draw", "max draw", "max send", // failed: MAX goes out
            "fhd draw", "max draw", "max send", // inside the backoff: no try
        ]
    );
    assert_eq!(max.ndi.newest_stamp(), None, "nothing read back");
    worker.serve(&plain(3, &a), t0 + MAX_RETRY_BACKOFF);
    assert_eq!(gpu.log().uyvy_enabled, 1, "the backoff over: on");
    assert_eq!(taken(&max, 3), Some(fake_uyvy(3)));
    assert_eq!(max.status().submitted, 3, "MAX never waited for NDI");
}

/// With MAX off NDI's compose is the compositor's only one: a failed build
/// is logged once (`Lines::ndi`, a WARN), nothing while it holds, and once
/// composes come back one INFO, after the log's window. With MAX on, MAX's
/// own line names it (`Lines::ndi` stays quiet).
#[test]
fn with_max_off_a_failing_compositor_is_logged_for_ndi() {
    let max = ndi_on(false);
    let gpu = FakeGpu::default();
    gpu.script().compositor.push_back(GpuError::NoAdapter);
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let a = picture(4, 2);
    let t0 = Instant::now();
    let lines = worker.serve_lines(&plain(1, &a), t0, t0);
    assert_eq!(lines.ndi, Some(LogLine::Failing { held_back: 0 }));
    assert_eq!(
        (lines.max, lines.fhd),
        (None, None),
        "MAX and FHD not wanted"
    );
    let lines = worker.serve_lines(&plain(2, &a), t0, t0);
    assert_eq!(lines.ndi, None, "still failing: said once");
    let back = t0 + MAX_RETRY_BACKOFF;
    let lines = worker.serve_lines(&plain(3, &a), back, back);
    assert_eq!(lines.ndi, None, "composes again, inside the log's window");
    assert_eq!(max.ndi.newest_stamp(), Some(3));
    let window = Duration::from_nanos(MAX_LOG_EVERY_100NS as u64 * 100);
    let later = t0 + window;
    let lines = worker.serve_lines(&plain(4, &a), later, later);
    assert_eq!(lines.ndi, Some(LogLine::Recovered { held_back: 1 }));

    let max = ndi_on(true);
    let gpu = FakeGpu::default();
    gpu.script().compositor.push_back(GpuError::NoAdapter);
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let lines = worker.serve_lines(&plain(1, &a), t0, t0);
    assert_eq!(lines.max, Some(LogLine::Failing { held_back: 0 }));
    assert_eq!(lines.ndi, None, "MAX's line names it");
}

/// The pass's log follows every boundary (review rounds 3-4): the retry
/// after the backoff lands inside the log's window, so that recovery is
/// said after the window, and a later failing spell with the same error
/// is WARNed again (never stuck on the old failure).
#[test]
fn a_quick_recovery_of_the_enable_is_said_after_the_window_and_a_new_failure_warned() {
    let max = ndi_on(true);
    let gpu = FakeGpu::default();
    gpu.script().enable_uyvy.push_back(gpu_refusal());
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let a = picture(4, 2);
    let t0 = Instant::now();
    let failing = PassLine::Failing(Arc::from(gpu_refusal().to_string()));
    let lines = worker.serve_lines(&plain(1, &a), t0, t0);
    assert_eq!(lines.ndi_pass, Some((failing.clone(), 0)));
    let retry = t0 + MAX_RETRY_BACKOFF;
    let lines = worker.serve_lines(&plain(2, &a), retry, retry);
    assert_eq!(lines.ndi_pass, None, "on again inside the window: held");
    let window = Duration::from_nanos(MAX_LOG_EVERY_100NS as u64 * 100);
    let after = t0 + window;
    let lines = worker.serve_lines(&plain(3, &a), after, after);
    assert_eq!(
        lines.ndi_pass,
        Some((PassLine::On, 1)),
        "said by the first boundary after the window"
    );
    // The device lost and rebuilt, and the enable refused again.
    let later = after + window;
    gpu.script().compose.push_back(device_lost());
    worker.serve_lines(&plain(4, &a), later, later);
    gpu.script().enable_uyvy.push_back(gpu_refusal());
    let lines = worker.serve_lines(&plain(5, &a), later, later);
    assert_eq!(gpu.log().compositors_built, 2, "rebuilt at once");
    assert_eq!(
        lines.ndi_pass,
        Some((failing, 0)),
        "the new failing spell is WARNed"
    );
}

/// The pass has ONE log (review round 4): its first enable says "on", a
/// pass rebuilt with the device says nothing new, and NDI's switch to the
/// canvas says "off", once. No line is written outside it.
#[test]
fn the_pass_log_says_on_and_off_once_and_nothing_for_a_rebuilt_pass() {
    let max = ndi_on(true);
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let a = picture(4, 2);
    let t0 = Instant::now();
    let lines = worker.serve_lines(&plain(1, &a), t0, t0);
    assert_eq!(lines.ndi_pass, Some((PassLine::On, 0)), "the first enable");
    let window = Duration::from_nanos(MAX_LOG_EVERY_100NS as u64 * 100);
    let t1 = t0 + window;
    gpu.script().compose.push_back(device_lost());
    let lines = worker.serve_lines(&plain(2, &a), t1, t1);
    assert_eq!(lines.ndi_pass, None, "still on until the loss");
    let lines = worker.serve_lines(&plain(3, &a), t1, t1);
    assert_eq!(gpu.log().uyvy_enabled, 2, "rebuilt and enabled again");
    assert_eq!(lines.ndi_pass, None, "on again: nothing new to say");
    max.set_ndi_source(NdiSource::Cpu);
    let t2 = t1 + window;
    let lines = worker.serve_lines(&plain(4, &a), t2, t2);
    assert_eq!(lines.ndi_pass, Some((PassLine::Off, 0)), "off");
    let lines = worker.serve_lines(&plain(5, &a), t2, t2);
    assert_eq!(lines.ndi_pass, None, "said once");
}

/// MAX off and the pass refused: nothing wants a compose (no output can go
/// out, NDI reads nothing back), so nothing is drawn.
#[test]
fn with_max_off_a_refused_pass_composes_nothing() {
    let max = ndi_on(false);
    let gpu = FakeGpu::default();
    gpu.script().enable_uyvy.push_back(gpu_refusal());
    let mut worker = MaxWorker::new(&max, gpu.clone());
    worker.serve(&black(1), Instant::now());
    let log = gpu.log();
    assert_eq!(log.order, ["ndi on"]);
    assert_eq!(log.composes, 0);
    drop(log);
    assert_eq!(max.ndi.newest_stamp(), None);
}

#[test]
fn a_failed_readback_pushes_nothing_and_costs_max_nothing() {
    let max = ndi_on(true);
    let gpu = FakeGpu::default();
    gpu.script().read_uyvy.push_back(device_lost());
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let a = picture(4, 2);
    let t0 = Instant::now();
    worker.serve(&plain(1, &a), t0);
    assert_eq!(max.ndi.newest_stamp(), None, "nothing pushed");
    let log = gpu.log();
    assert_eq!((log.sent, log.drops.len()), (1, 0), "MAX went out");
    drop(log);
    worker.serve(&plain(2, &a), t0);
    assert_eq!(taken(&max, 2), Some(fake_uyvy(2)));
}

#[test]
fn a_lost_device_pushes_nothing_and_the_rebuilt_compositor_reads_back_again() {
    let max = ndi_on(true);
    let gpu = FakeGpu::default();
    gpu.script().compose.push_back(device_lost());
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let a = picture(4, 2);
    let t0 = Instant::now();
    worker.serve(&plain(1, &a), t0);
    assert_eq!(max.ndi.newest_stamp(), None, "nothing pushed");
    assert_eq!(gpu.log().drops, ["sender", "compositor"]);
    worker.serve(&plain(2, &a), t0);
    let log = gpu.log();
    assert_eq!(log.compositors_built, 2, "rebuilt at once");
    assert_eq!(log.uyvy_enabled, 2, "the pass with it");
    drop(log);
    assert_eq!(taken(&max, 2), Some(fake_uyvy(2)));
}

/// With MAX off, NDI's readback is the only boundary that "goes out": it
/// proves the device works, so a later loss rebuilds at once — like a
/// sent MAX boundary (`lost_unsent`). A failed readback proves nothing.
#[test]
fn an_ndi_readback_proves_the_device_and_a_failed_one_does_not() {
    let a = picture(4, 2);
    let t0 = Instant::now();

    // Lost, rebuilt and read back: the next loss rebuilds at once.
    let max = ndi_on(false);
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    gpu.script().compose.push_back(device_lost());
    worker.serve(&plain(1, &a), t0);
    worker.serve(&plain(2, &a), t0);
    gpu.script().compose.push_back(device_lost());
    worker.serve(&plain(3, &a), t0);
    worker.serve(&plain(4, &a), t0);
    assert_eq!(gpu.log().compositors_built, 3, "every loss rebuilt at once");
    assert_eq!(max.ndi.newest_stamp(), Some(4));

    // Lost, rebuilt, its readback failed: the next loss waits the backoff.
    let max = ndi_on(false);
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    gpu.script().compose.push_back(device_lost());
    worker.serve(&plain(1, &a), t0);
    gpu.script().read_uyvy.push_back(gpu_refusal());
    worker.serve(&plain(2, &a), t0);
    gpu.script().compose.push_back(device_lost());
    worker.serve(&plain(3, &a), t0);
    worker.serve(&plain(4, &a), t0);
    assert_eq!(
        gpu.log().compositors_built,
        2,
        "a second loss with nothing out since waits the backoff"
    );
    assert_eq!(max.ndi.newest_stamp(), None);
    worker.serve(&plain(5, &a), t0 + MAX_RETRY_BACKOFF);
    assert_eq!(gpu.log().compositors_built, 3);
    assert_eq!(max.ndi.newest_stamp(), Some(5));
}

/// A job served when nothing wants the compositor any more (MAX off and
/// NDI on the canvas: a setting changed after the thread took it) builds
/// nothing and composes nothing. MAX's 4K target and its `SP-program-MAX`
/// Spout sender never appear for one stray boundary while MAX is off; the
/// thread's next step releases the GPU.
#[test]
fn a_job_served_when_nothing_wants_the_compositor_builds_nothing() {
    let a = picture(4, 2);
    let t0 = Instant::now();
    let max = MaxOut::new();
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let lines = worker.serve_lines(&plain(1, &a), t0, t0);
    assert_eq!((lines.max, lines.fhd, lines.ndi), (None, None, None));
    let log = gpu.log();
    assert_eq!(
        (
            log.compositors_built,
            log.senders_built,
            log.composes,
            log.sent
        ),
        (0, 0, 0, 0),
        "MAX off, NDI on the canvas: nothing is built"
    );
    drop(log);
    assert!(!worker.holds_gpu());

    // NDI from the GPU with MAX off, then NDI back on the canvas while a
    // job is in flight.
    let max = ndi_on(false);
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    worker.serve(&plain(1, &a), t0);
    max.set_ndi_source(NdiSource::Cpu);
    worker.serve(&plain(2, &a), t0);
    let log = gpu.log();
    assert!(log.targets_added.is_empty(), "no MAX target");
    assert_eq!(
        (log.senders_built, log.sent, log.composes),
        (0, 0, 1),
        "boundary 1 composed for NDI alone, boundary 2 not at all"
    );
    drop(log);
    assert_eq!(max.status().submitted, 0, "nothing sent as MAX");
}

/// A release (nothing wants the compositor, a quiesce, the thread's end)
/// leaves the read-back pictures in the feed (review round 5 F1): the
/// `SP-program` sender may still flush its held pair with the newest one,
/// and it lets them go itself once NDI is on the canvas.
#[test]
fn a_release_leaves_the_read_back_pictures_to_the_sender() {
    let max = ndi_on(false);
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    worker.serve(&plain(1, &picture(4, 2)), Instant::now());
    assert_eq!(max.ndi.newest_stamp(), Some(1));
    worker.release(Instant::now());
    assert_eq!(gpu.log().drops, ["compositor"], "released");
    assert_eq!(taken(&max, 1), Some(fake_uyvy(1)), "the picture stays");
}

/// Review round 5 F2: with MAX off nothing composes once NDI is on the
/// canvas (`serve_lines` returns before the pass), so the thread's release
/// is where the pass's log says "off"; NDI back on the GPU is then news.
#[test]
fn with_max_off_the_release_says_the_pass_is_off() {
    let max = ndi_on(false);
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let a = picture(4, 2);
    let t0 = Instant::now();
    let window = Duration::from_nanos(MAX_LOG_EVERY_100NS as u64 * 100);
    let lines = worker.serve_lines(&plain(1, &a), t0, t0);
    assert_eq!(lines.ndi_pass, Some((PassLine::On, 0)));
    max.set_ndi_source(NdiSource::Cpu);
    let t1 = t0 + window;
    let lines = worker.serve_lines(&plain(2, &a), t1, t1);
    assert_eq!(lines.ndi_pass, None, "nothing wants the compositor");
    worker.release(t1);
    max.set_ndi_source(NdiSource::Gpu);
    let t2 = t1 + window;
    let lines = worker.serve_lines(&plain(3, &a), t2, t2);
    assert_eq!(
        lines.ndi_pass,
        Some((PassLine::On, 0)),
        "the release said off, so on again is said"
    );
}

/// Review round 5 B5-1: with MAX off nothing composes before the pass is
/// on, so a device lost at the pass's enable is lost right there, like one
/// lost while building: the compositor goes, and after the backoff a new
/// one is built, its pass enabled and read back. (The removed compositor
/// used to be kept, its enable failing every backoff for good.)
#[test]
fn with_max_off_a_device_lost_at_the_pass_s_enable_is_rebuilt_after_the_backoff() {
    let max = ndi_on(false);
    let gpu = FakeGpu::default();
    gpu.script().enable_uyvy.push_back(device_lost());
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let a = picture(4, 2);
    let t0 = Instant::now();
    let lines = worker.serve_lines(&plain(1, &a), t0, t0);
    let log = gpu.log();
    assert_eq!(log.drops, ["compositor"], "lost at the enable");
    assert_eq!(log.composes, 0);
    drop(log);
    assert_eq!(lines.ndi, Some(LogLine::Failing { held_back: 0 }));
    let lost = PassLine::Failing(Arc::from(device_lost().to_string()));
    assert_eq!(lines.ndi_pass, Some((lost, 0)), "the pass's own line");
    assert_eq!(max.status().device_resets, 1);
    let early = t0 + MAX_RETRY_BACKOFF - Duration::from_millis(1);
    worker.serve(&plain(2, &a), early);
    assert_eq!(gpu.log().compositors_built, 1, "the backoff runs");
    worker.serve(&plain(3, &a), t0 + MAX_RETRY_BACKOFF);
    let log = gpu.log();
    assert_eq!(
        (log.compositors_built, log.uyvy_enabled),
        (2, 1),
        "rebuilt, the pass on"
    );
    drop(log);
    assert_eq!(taken(&max, 3), Some(fake_uyvy(1)), "read back");
}

/// Review round 6: a device lost at the enable of a compositor that was
/// already up (NDI flipped to the GPU while MAX runs) is lost like one a
/// compose finds. The boundary fails (MAX's too: one event, no refused
/// sender, so no MAX backoff), and since a boundary went out after the
/// last loss the next job rebuilds at once, the pass with it. Only a
/// compositor built for the boundary (the test above) waits the backoff.
#[test]
fn a_device_lost_at_the_enable_of_a_running_compositor_is_rebuilt_at_once() {
    let max = MaxOut::new();
    max.set_enabled(true);
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let a = picture(4, 2);
    let t0 = Instant::now();
    worker.serve(&plain(1, &a), t0);
    assert_eq!(gpu.log().sent, 1, "MAX went out: the device works");
    max.set_ndi_source(NdiSource::Gpu);
    gpu.script().enable_uyvy.push_back(device_lost());
    let lines = worker.serve_lines(&plain(2, &a), t0, t0);
    let lost = PassLine::Failing(Arc::from(device_lost().to_string()));
    assert_eq!(lines.ndi_pass, Some((lost, 0)));
    assert_eq!(lines.max, Some(LogLine::Failing { held_back: 0 }));
    assert_eq!(gpu.log().drops, ["sender", "compositor"]);
    let status = max.status();
    assert_eq!(
        (status.device_resets, status.failed, status.sender_backoffs),
        (1, 1, 0),
        "one device event, no MAX backoff"
    );
    worker.serve(&plain(3, &a), t0);
    let log = gpu.log();
    assert_eq!(
        (log.compositors_built, log.uyvy_enabled, log.sent),
        (2, 1, 2),
        "rebuilt at the next job, the pass on, MAX out again"
    );
    drop(log);
    assert_eq!(taken(&max, 3), Some(fake_uyvy(2)), "read back");
}

/// A job the hand-off coalesced never reaches the thread: its boundary is
/// never read back, and the `SP-program` sender repeats over the gap.
#[test]
fn a_coalesced_job_leaves_a_gap_in_the_feed() {
    let max = ndi_on(false);
    let _consumer = max.attach();
    for stamp in 1..=3 {
        assert!(max.offer_with(|| black(stamp)));
    }
    assert_eq!(max.status().coalesced, 1, "boundary 1 dropped");
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    while let Some(crate::playback::program_max::MaxNext::Job(job, offered)) =
        max.try_next(worker.holds_gpu())
    {
        worker.serve_offered(&job, offered, offered);
    }
    assert!(max.ndi.take(1).is_none(), "the gap");
    assert!(max.ndi.take(2).is_some());
    assert!(max.ndi.take(3).is_some());
}
