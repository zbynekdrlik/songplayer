//! #223 S2: the `program-max` thread's release and its loop, on the fake
//! GPU: a release drops the sender before the compositor and forgets the
//! backoff and the pictures it held, the production GPU off Windows, and the
//! loop serving jobs, releasing while off and ending on a stop. Moved out of
//! `program_max_worker_tests.rs` for the 1000-line cap (#279 P1b).
//! Wired via `#[cfg(test)] #[path = "program_max_worker_tests_release.rs"] mod tests_release;`.

use std::sync::Arc;
use std::time::Instant;

use super::MaxWorker;
use super::tests::{Drawn, FakeGpu, not_registered, pic, picture, plain, spawn_loop, wait_until};
use crate::playback::program_max::{MAX_NOT_RUNNING, MaxJob, MaxOut};

#[test]
fn release_drops_the_sender_then_the_compositor_and_forgets_the_backoff() {
    let max = MaxOut::new();
    let gpu = FakeGpu::default();
    gpu.script().send.push_back(not_registered());
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&MaxJob::Black { stamp_100ns: 1 }, t0);
    assert_eq!(gpu.log().drops, ["sender"]);
    assert!(
        worker.holds_gpu(),
        "a compositor without a sender is still held"
    );
    worker.release();
    assert!(!worker.holds_gpu());
    assert_eq!(gpu.log().drops, ["sender", "compositor"]);
    worker.serve(&MaxJob::Black { stamp_100ns: 2 }, t0);
    let log = gpu.log();
    assert_eq!(
        (log.compositors_built, log.senders_built, log.sent),
        (2, 2, 1),
        "switched back on: built at once, the backoff is forgotten"
    );
    drop(log);
    worker.release();
    assert_eq!(
        gpu.log().drops,
        ["sender", "compositor", "sender", "compositor"],
        "a release drops the sender before the compositor"
    );
}

#[test]
fn a_release_forgets_the_pictures_it_held() {
    let max = MaxOut::new();
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let a = picture(4, 2);
    let t0 = Instant::now();
    worker.serve(&plain(1, &a), t0);
    worker.release();
    worker.serve(&plain(2, &a), t0);
    assert_eq!(
        gpu.log().drawn,
        [Drawn::Picture(pic(1, &a)), Drawn::Picture(pic(2, &a))],
        "the frame is no longer held after the release, so it is a new id"
    );
}

/// Off Windows the production GPU has no Direct3D: unsupported, once.
#[cfg(not(windows))]
#[test]
fn off_windows_the_production_gpu_is_unsupported() {
    use super::MaxGpu;
    use super::SpoutGpu;
    assert!(matches!(
        SpoutGpu.compositor(),
        Err(sp_gpu::GpuError::Unsupported)
    ));
    let max = MaxOut::new();
    let mut worker = MaxWorker::new(&max, SpoutGpu);
    worker.serve(&MaxJob::Black { stamp_100ns: 1 }, Instant::now());
    let status = max.status();
    assert_eq!((status.state.as_str(), status.failed), ("unsupported", 0));
    assert!(!worker.holds_gpu());
}

#[test]
fn the_loop_serves_jobs_releases_while_off_and_exits_on_stop() {
    let max = Arc::new(MaxOut::new());
    max.set_enabled(true);
    let gpu = FakeGpu::default();
    let thread = spawn_loop(&max, &gpu);
    wait_until("the loop takes jobs", || max.accepting());
    assert_eq!(max.status().state, "running");
    let a = picture(4, 2);
    max.offer_with(|| plain(1, &a));
    wait_until("the first job went out", || max.status().submitted == 1);
    assert_eq!(gpu.log().drawn, [Drawn::Picture(pic(1, &a))]);

    max.set_enabled(false);
    wait_until("off releases the GPU", || gpu.log().drops.len() == 2);
    assert_eq!(gpu.log().drops, ["sender", "compositor"]);
    assert_eq!(max.status().state, "off");

    max.set_enabled(true);
    max.offer_with(|| plain(2, &a));
    wait_until("back on: rebuilt and sent", || max.status().submitted == 2);
    assert_eq!(gpu.log().compositors_built, 2);

    max.stop();
    // Bounded: a stop that does not end the loop fails here, never hangs.
    wait_until("the loop exits on stop", || thread.is_finished());
    thread.join().expect("the loop");
    assert_eq!(
        gpu.log().drops,
        ["sender", "compositor", "sender", "compositor"],
        "its end releases the GPU"
    );
    assert_eq!(max.status().state, format!("error: {MAX_NOT_RUNNING}"));
}
