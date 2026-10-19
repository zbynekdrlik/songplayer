//! #223 S2: the `program-max` thread's release and its loop, on the fake
//! GPU: a release drops the sender before the compositor and forgets the
//! backoff and the pictures it held, the production GPU off Windows, and the
//! loop serving jobs, releasing while off and ending on a stop. Moved out of
//! `program_max_worker_tests.rs` for the 1000-line cap (#279 P1b).
//! Wired via `#[cfg(test)] #[path = "program_max_worker_tests_release.rs"] mod tests_release;`.

use std::sync::Arc;
use std::time::Instant;

use sp_gpu::{MAX_FIT, MAX_TARGET, Target};

use super::tests::{Drawn, FakeGpu, not_registered, pic, picture, plain, spawn_loop, wait_until};
use super::{MAX_RETRY_BACKOFF, MaxWorker};
use crate::playback::program_max::{MAX_NOT_RUNNING, MaxJob, MaxOut};

#[test]
fn release_drops_the_sender_then_the_compositor_and_forgets_the_backoff() {
    let max = MaxOut::new();
    max.set_enabled(true);
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
    worker.release(Instant::now());
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
    worker.release(Instant::now());
    assert_eq!(
        gpu.log().drops,
        ["sender", "compositor", "sender", "compositor"],
        "a release drops the sender before the compositor"
    );
}

#[test]
fn a_release_forgets_the_pictures_it_held() {
    let max = MaxOut::new();
    max.set_enabled(true);
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let a = picture(4, 2);
    let t0 = Instant::now();
    worker.serve(&plain(1, &a), t0);
    worker.release(Instant::now());
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
    max.set_enabled(true);
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

/// #251 (19.10.2026): a changed `program_max_size` drops MAX's sender, then
/// its target, and the same boundary builds both anew at the new size; the
/// same size again changes nothing, and back to the canvas makes them anew
/// once more.
#[test]
fn a_changed_size_makes_max_anew_at_it() {
    let max = MaxOut::new();
    max.set_enabled(true);
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&MaxJob::Black { stamp_100ns: 1 }, t0);
    assert!(max.set_max_size((1920, 1080)), "a change");
    assert!(!max.set_max_size((1920, 1080)), "the same again");
    worker.serve(&MaxJob::Black { stamp_100ns: 2 }, t0);
    worker.serve(&MaxJob::Black { stamp_100ns: 3 }, t0);
    let fhd_sized = Target {
        width: 1920,
        height: 1080,
        fit: MAX_FIT,
    };
    {
        let log = gpu.log();
        assert_eq!(log.targets_added, [MAX_TARGET, fhd_sized]);
        assert_eq!(log.max_sender_targets, [MAX_TARGET, fhd_sized]);
        assert_eq!(log.drops, ["sender"], "the old sender, once");
        assert_eq!(
            (log.targets_removed, log.senders_built, log.sent),
            (1, 2, 3),
            "made anew once, every boundary sent"
        );
        assert_eq!(
            log.order,
            [
                "fhd draw",
                "max draw",
                "max send",
                "max target removed",
                "fhd draw",
                "max draw",
                "max send",
                "fhd draw",
                "max draw",
                "max send",
            ],
            "the old target off before the boundary that draws the new one"
        );
    }
    assert!(max.set_max_size((3840, 2160)));
    worker.serve(&MaxJob::Black { stamp_100ns: 4 }, t0);
    let log = gpu.log();
    assert_eq!(log.targets_added, [MAX_TARGET, fhd_sized, MAX_TARGET]);
    assert_eq!((log.targets_removed, log.senders_built), (2, 3));
}

/// #251: a size change while MAX waits out a refused sender's backoff (its
/// target already off the compositor) keeps the backoff; the build after it
/// is at the new size.
#[test]
fn a_size_change_in_a_backoff_waits_it_out() {
    let max = MaxOut::new();
    max.set_enabled(true);
    let gpu = FakeGpu::default();
    gpu.script().send.push_back(not_registered());
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&MaxJob::Black { stamp_100ns: 1 }, t0);
    assert_eq!(gpu.log().targets_removed, 1, "refused: its target is off");
    assert!(max.set_max_size((1920, 1080)));
    worker.serve(&MaxJob::Black { stamp_100ns: 2 }, t0);
    assert_eq!(gpu.log().senders_built, 1, "the backoff holds");
    worker.serve(&MaxJob::Black { stamp_100ns: 3 }, t0 + MAX_RETRY_BACKOFF);
    let log = gpu.log();
    assert_eq!(
        log.targets_added,
        [
            MAX_TARGET,
            Target {
                width: 1920,
                height: 1080,
                fit: MAX_FIT,
            },
        ]
    );
    assert_eq!((log.senders_built, log.sent), (2, 1));
    assert_eq!(log.max_sender_targets[1].width, 1920, "the new sender's");
}
