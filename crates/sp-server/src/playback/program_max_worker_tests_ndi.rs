//! #279 P1b: the `program-max` thread reads NDI `SP-program`'s picture
//! back, on the fake GPU: the UYVY pass on the compositor's primary (FHD)
//! target while NDI comes from the GPU, its readback pushed into the feed
//! after the compose and BEFORE MAX's send, a Black boundary's cached black
//! (nothing mapped), MAX off composing the FHD target alone, the pass off
//! with NDI, a failed enable's backoff, and what a failed readback, a lost
//! device or a coalesced job leave in the feed (nothing: the `SP-program`
//! sender repeats).
//! Wired via `#[cfg(test)] #[path = "program_max_worker_tests_ndi.rs"] mod tests_ndi;`.

use std::time::{Duration, Instant};

use sp_core::config::NdiSource;
use sp_gpu::{GpuError, MAX_TARGET};

use super::tests::{
    Drawn, FakeGpu, UYVY_US, device_lost, fake_uyvy, not_registered, pic, picture, plain,
};
use super::{MAX_RETRY_BACKOFF, MaxWorker};
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
