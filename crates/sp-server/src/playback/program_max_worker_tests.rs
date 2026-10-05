//! #223 S2: the `program-max` thread's decisions, on a fake GPU: what it
//! builds and when, the picture ids, what a lost device, a refused sender, a
//! failed build or a lost frame cost, the backoff, the release, and the loop.
//! The fake ([`FakeGpu`]) is `pub(crate)`: `program_output_tests_max.rs`
//! holds its compose to stall the consumer.
//! Wired via `#[cfg(test)] #[path = "program_max_worker_tests.rs"] pub(crate) mod tests;`.

use std::collections::VecDeque;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use sp_gpu::{ComposeStats, Composition, GpuError, Nv12Picture, PictureError, SpoutSendStats};

use super::{
    MAX_RETRY_BACKOFF, MaxCompositor, MaxGpu, MaxSender, MaxWorker, PictureIds, is_refusal,
    run_max_loop,
};
use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_max::{MAX_NOT_RUNNING, MaxJob, MaxOut, MaxPicture};

/// A gate a held compose waits behind.
#[derive(Default)]
pub(crate) struct Gate {
    open: Mutex<bool>,
    opened: Condvar,
}

impl Gate {
    pub(crate) fn open(&self) {
        *self.open.lock().unwrap_or_else(|p| p.into_inner()) = true;
        self.opened.notify_all();
    }

    /// Wait until the gate opens (bounded, so no bug hangs the binary).
    fn wait(&self) {
        let open = self.open.lock().unwrap_or_else(|p| p.into_inner());
        let _open = self
            .opened
            .wait_timeout_while(open, Duration::from_secs(60), |open| !*open)
            .unwrap_or_else(|p| p.into_inner());
    }
}

/// One picture as the fake compositor was given it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Pic {
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub len: usize,
}

impl Pic {
    fn of(picture: &Nv12Picture<'_>) -> Self {
        Self {
            id: picture.id,
            width: picture.width,
            height: picture.height,
            stride: picture.stride,
            len: picture.data.len(),
        }
    }
}

/// What one compose was given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Drawn {
    Black,
    Picture(Pic),
    Fade(Option<Pic>, Option<Pic>, u32),
}

/// What the fake GPU did.
#[derive(Debug, Default)]
pub(crate) struct Log {
    pub compositors_built: u32,
    pub senders_built: u32,
    /// "sender" / "compositor", in drop order.
    pub drops: Vec<&'static str>,
    pub drawn: Vec<Drawn>,
    pub sent: u32,
}

/// The fake GPU's next results; an empty queue answers `Ok`.
#[derive(Default)]
pub(crate) struct Script {
    pub compositor: VecDeque<GpuError>,
    pub sender: VecDeque<GpuError>,
    pub compose: VecDeque<GpuError>,
    pub send: VecDeque<GpuError>,
}

/// A held compose: it says it entered, then waits for the gate.
pub(crate) struct Hold {
    pub entered: mpsc::Sender<()>,
    pub gate: Arc<Gate>,
}

#[derive(Default)]
pub(crate) struct FakeShared {
    log: Mutex<Log>,
    script: Mutex<Script>,
    hold: Mutex<Option<Hold>>,
}

/// The stats every fake compose and send report.
pub(crate) const UPLOAD_US: u64 = 11;
pub(crate) const DRAW_US: u64 = 22;
pub(crate) const SEND_US: u64 = 33;

/// The fake GPU: its compositor and sender record into [`Log`] and answer
/// from [`Script`]. Clones share one state.
#[derive(Clone, Default)]
pub(crate) struct FakeGpu(Arc<FakeShared>);

impl FakeGpu {
    pub(crate) fn log(&self) -> MutexGuard<'_, Log> {
        self.0.log.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(crate) fn script(&self) -> MutexGuard<'_, Script> {
        self.0.script.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Hold the next compose behind `hold`.
    pub(crate) fn hold_next_compose(&self, hold: Hold) {
        *self.0.hold.lock().unwrap_or_else(|p| p.into_inner()) = Some(hold);
    }
}

pub(crate) struct FakeCompositor(Arc<FakeShared>);
pub(crate) struct FakeSender(Arc<FakeShared>);

fn log_of(shared: &FakeShared) -> MutexGuard<'_, Log> {
    shared.log.lock().unwrap_or_else(|p| p.into_inner())
}

fn next_error(
    shared: &FakeShared,
    pick: impl Fn(&mut Script) -> Option<GpuError>,
) -> Option<GpuError> {
    let mut script = shared.script.lock().unwrap_or_else(|p| p.into_inner());
    // A generic `Fn`'s argument does not deref-coerce: pass the `Script`.
    pick(&mut *script)
}

impl Drop for FakeCompositor {
    fn drop(&mut self) {
        log_of(&self.0).drops.push("compositor");
    }
}

impl Drop for FakeSender {
    fn drop(&mut self) {
        log_of(&self.0).drops.push("sender");
    }
}

impl MaxGpu for FakeGpu {
    type Compositor = FakeCompositor;
    type Sender = FakeSender;

    fn compositor(&mut self) -> Result<FakeCompositor, GpuError> {
        log_of(&self.0).compositors_built += 1;
        match next_error(&self.0, |s| s.compositor.pop_front()) {
            Some(error) => Err(error),
            None => Ok(FakeCompositor(self.0.clone())),
        }
    }

    fn sender(&mut self, _compositor: &FakeCompositor) -> Result<FakeSender, GpuError> {
        log_of(&self.0).senders_built += 1;
        match next_error(&self.0, |s| s.sender.pop_front()) {
            Some(error) => Err(error),
            None => Ok(FakeSender(self.0.clone())),
        }
    }
}

impl MaxCompositor for FakeCompositor {
    fn compose(&mut self, composition: &Composition<'_>) -> Result<ComposeStats, GpuError> {
        let hold = self.0.hold.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(hold) = hold {
            let _ = hold.entered.send(());
            hold.gate.wait();
        }
        let drawn = match composition {
            Composition::Black => Drawn::Black,
            Composition::Picture(picture) => Drawn::Picture(Pic::of(picture)),
            Composition::Fade {
                from,
                to,
                weight_q8,
            } => Drawn::Fade(
                from.as_ref().map(Pic::of),
                to.as_ref().map(Pic::of),
                *weight_q8,
            ),
        };
        log_of(&self.0).drawn.push(drawn);
        match next_error(&self.0, |s| s.compose.pop_front()) {
            Some(error) => Err(error),
            None => Ok(ComposeStats {
                upload_us: UPLOAD_US,
                draw_us: DRAW_US,
                uploads: 1,
            }),
        }
    }
}

impl MaxSender for FakeSender {
    fn send(&mut self) -> Result<SpoutSendStats, GpuError> {
        match next_error(&self.0, |s| s.send.pop_front()) {
            Some(error) => Err(error),
            None => {
                log_of(&self.0).sent += 1;
                Ok(SpoutSendStats { send_us: SEND_US })
            }
        }
    }
}

/// A `width`×`height` NV12 picture (stride = width) of its own allocation.
pub(crate) fn picture(width: u32, height: u32) -> MaxPicture {
    MaxPicture {
        width,
        height,
        stride: width,
        video: SharedFrame::new(vec![80; (width * height * 3 / 2) as usize]),
    }
}

fn plain(stamp: i64, picture: &MaxPicture) -> MaxJob {
    MaxJob::Picture {
        stamp_100ns: stamp,
        picture: picture.clone(),
    }
}

fn pic(id: u64, picture: &MaxPicture) -> Pic {
    Pic {
        id,
        width: picture.width,
        height: picture.height,
        stride: picture.stride,
        len: picture.video.len(),
    }
}

fn device_lost() -> GpuError {
    GpuError::DeviceLost {
        call: "Present",
        hresult: 0x887A_0005,
    }
}

fn not_registered() -> GpuError {
    GpuError::SpoutNotRegistered {
        name: "SP-program-MAX".into(),
        why: "test",
    }
}

/// The ids `composition` labelled, in draw order (`None` = a missing side).
fn ids(composition: &Composition<'_>) -> Vec<Option<u64>> {
    match composition {
        Composition::Black => Vec::new(),
        Composition::Picture(p) => vec![Some(p.id)],
        Composition::Fade { from, to, .. } => vec![from.map(|p| p.id), to.map(|p| p.id)],
    }
}

#[test]
fn a_picture_keeps_its_id_while_the_last_boundary_showed_it() {
    let (a, b) = (picture(4, 2), picture(8, 2));
    let mut labels = PictureIds::default();
    assert_eq!(ids(&labels.composition(&plain(1, &a))), [Some(1)]);
    assert_eq!(
        ids(&labels.composition(&plain(2, &a))),
        [Some(1)],
        "the same allocation again (a held frame): the same id, no upload"
    );
    let same_bytes = MaxPicture {
        video: SharedFrame::new(a.video.to_vec()),
        ..a.clone()
    };
    assert_eq!(
        ids(&labels.composition(&plain(3, &same_bytes))),
        [Some(2)],
        "equal bytes in another allocation: a new id"
    );
    let fade = MaxJob::Fade {
        stamp_100ns: 4,
        from: Some(same_bytes.clone()),
        to: Some(b.clone()),
        weight_q8: 64,
    };
    let composition = labels.composition(&fade);
    assert_eq!(ids(&composition), [Some(2), Some(3)]);
    let Composition::Fade { weight_q8, .. } = composition else {
        panic!("a fade composes a fade: {composition:?}");
    };
    assert_eq!(weight_q8, 64);
    assert_eq!(
        ids(&labels.composition(&plain(5, &b))),
        [Some(3)],
        "the fade's incoming side, now plain"
    );
    assert_eq!(
        ids(&labels.composition(&plain(6, &a))),
        [Some(4)],
        "a picture two boundaries back is not held: a new id"
    );
}

#[test]
fn both_sides_of_one_allocation_share_an_id_and_black_holds_nothing() {
    let a = picture(4, 2);
    let mut labels = PictureIds::default();
    let both = MaxJob::Fade {
        stamp_100ns: 1,
        from: Some(a.clone()),
        to: Some(a.clone()),
        weight_q8: 128,
    };
    assert_eq!(ids(&labels.composition(&both)), [Some(1), Some(1)]);
    let half = MaxJob::Fade {
        stamp_100ns: 2,
        from: None,
        to: Some(a.clone()),
        weight_q8: 128,
    };
    assert_eq!(ids(&labels.composition(&half)), [None, Some(1)]);
    let black = labels.composition(&MaxJob::Black { stamp_100ns: 3 });
    assert!(matches!(black, Composition::Black));
    assert_eq!(
        ids(&labels.composition(&plain(4, &a))),
        [Some(2)],
        "the black boundary held nothing"
    );
}

#[test]
fn a_labelled_picture_is_the_jobs_own_bytes() {
    let a = picture(6, 4);
    let mut labels = PictureIds::default();
    let job = plain(1, &a);
    let Composition::Picture(p) = labels.composition(&job) else {
        panic!("a picture composes a picture");
    };
    assert_eq!(p.data.as_ptr(), a.video.as_ptr());
    assert_eq!((p.width, p.height, p.stride, p.data.len()), (6, 4, 6, 36));
}

#[test]
fn a_refusal_is_a_taken_or_unregistered_name_only() {
    assert!(is_refusal(&not_registered()));
    assert!(is_refusal(&GpuError::SpoutNameTaken {
        name: "SP-program-MAX".into()
    }));
    for other in [
        device_lost(),
        GpuError::NoAdapter,
        GpuError::Unsupported,
        GpuError::Spout {
            call: "spout_sender_send",
            code: 3,
        },
        GpuError::Picture(PictureError::Empty {
            width: 0,
            height: 0,
        }),
    ] {
        assert!(!is_refusal(&other), "{other:?}");
    }
}

#[test]
fn the_first_job_builds_both_on_the_thread_then_composes_and_sends() {
    let max = MaxOut::new();
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    assert!(!worker.holds_gpu(), "nothing built before a job");
    let a = picture(4, 2);
    let t0 = Instant::now();
    worker.serve(&plain(1, &a), t0);
    worker.serve(&MaxJob::Black { stamp_100ns: 2 }, t0);
    assert!(worker.holds_gpu());
    let log = gpu.log();
    assert_eq!((log.compositors_built, log.senders_built), (1, 1));
    assert_eq!(log.drawn, [Drawn::Picture(pic(1, &a)), Drawn::Black]);
    assert_eq!(log.sent, 2);
    drop(log);
    let status = max.status();
    assert_eq!((status.submitted, status.failed), (2, 0));
    assert_eq!(
        (status.upload_us_p99, status.draw_us_p99, status.send_us_p99),
        (UPLOAD_US, DRAW_US, SEND_US)
    );
}

#[test]
fn a_device_lost_at_compose_drops_both_and_rebuilds_on_the_next_job() {
    let max = MaxOut::new();
    max.set_enabled(true);
    let gpu = FakeGpu::default();
    gpu.script().compose.push_back(device_lost());
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let a = picture(4, 2);
    let t0 = Instant::now();
    worker.serve(&plain(1, &a), t0);
    assert!(!worker.holds_gpu());
    assert_eq!(
        gpu.log().drops,
        ["sender", "compositor"],
        "the sender first"
    );
    let status = max.status();
    assert_eq!((status.device_resets, status.failed), (1, 1));
    assert_eq!(status.state, format!("error: {}", device_lost()));

    worker.serve(&plain(2, &a), t0);
    let log = gpu.log();
    assert_eq!(
        (log.compositors_built, log.senders_built, log.sent),
        (2, 2, 1),
        "rebuilt on the very next job: no backoff after a lost device"
    );
    drop(log);
    let status = max.status();
    assert_eq!((status.state.as_str(), status.submitted), ("running", 1));
}

#[test]
fn a_device_lost_at_send_drops_both_too() {
    let max = MaxOut::new();
    let gpu = FakeGpu::default();
    gpu.script().send.push_back(device_lost());
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&MaxJob::Black { stamp_100ns: 1 }, t0);
    assert_eq!(gpu.log().drops, ["sender", "compositor"]);
    assert_eq!(max.status().device_resets, 1);
    worker.serve(&MaxJob::Black { stamp_100ns: 2 }, t0);
    assert_eq!(gpu.log().sent, 1);
}

#[test]
fn a_refused_sender_is_dropped_and_a_new_one_waits_the_backoff() {
    let max = MaxOut::new();
    let gpu = FakeGpu::default();
    gpu.script().send.push_back(not_registered());
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&MaxJob::Black { stamp_100ns: 1 }, t0);
    assert_eq!(gpu.log().drops, ["sender"], "the compositor stays");
    assert!(worker.holds_gpu());
    let status = max.status();
    assert_eq!((status.sender_backoffs, status.device_resets), (1, 0));

    let just_before = t0 + MAX_RETRY_BACKOFF - Duration::from_millis(1);
    worker.serve(&MaxJob::Black { stamp_100ns: 2 }, just_before);
    let log = gpu.log();
    assert_eq!(
        (log.senders_built, log.drawn.len()),
        (1, 1),
        "inside the backoff: no new sender, nothing composed"
    );
    drop(log);
    let status = max.status();
    assert_eq!(status.failed, 2, "the refusal and the skipped boundary");
    assert_eq!(status.state, format!("error: {}", not_registered()));

    worker.serve(&MaxJob::Black { stamp_100ns: 3 }, t0 + MAX_RETRY_BACKOFF);
    let log = gpu.log();
    assert_eq!(
        (log.compositors_built, log.senders_built, log.sent),
        (1, 2, 1),
        "at the backoff's end: a new sender on the same compositor"
    );
    assert_eq!(MAX_RETRY_BACKOFF, Duration::from_secs(3));
}

#[test]
fn a_name_taken_at_create_waits_the_backoff_too() {
    let max = MaxOut::new();
    let gpu = FakeGpu::default();
    gpu.script().sender.push_back(GpuError::SpoutNameTaken {
        name: "SP-program-MAX".into(),
    });
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&MaxJob::Black { stamp_100ns: 1 }, t0);
    assert_eq!(max.status().sender_backoffs, 1);
    assert!(gpu.log().drops.is_empty(), "the compositor stays");
    worker.serve(
        &MaxJob::Black { stamp_100ns: 2 },
        t0 + Duration::from_secs(1),
    );
    assert_eq!(gpu.log().senders_built, 1, "inside the backoff");
    worker.serve(&MaxJob::Black { stamp_100ns: 3 }, t0 + MAX_RETRY_BACKOFF);
    let log = gpu.log();
    assert_eq!(
        (log.compositors_built, log.senders_built, log.sent),
        (1, 2, 1)
    );
}

#[test]
fn a_failed_build_is_retried_after_the_backoff_not_on_every_job() {
    let max = MaxOut::new();
    max.set_enabled(true);
    let gpu = FakeGpu::default();
    gpu.script().compositor.push_back(GpuError::NoAdapter);
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&MaxJob::Black { stamp_100ns: 1 }, t0);
    let status = max.status();
    assert_eq!(status.state, format!("error: {}", GpuError::NoAdapter));
    assert_eq!(
        (status.failed, status.sender_backoffs, status.device_resets),
        (1, 0, 0)
    );
    worker.serve(&MaxJob::Black { stamp_100ns: 2 }, t0);
    assert_eq!(gpu.log().compositors_built, 1, "no build on the next job");
    worker.serve(&MaxJob::Black { stamp_100ns: 3 }, t0 + MAX_RETRY_BACKOFF);
    let log = gpu.log();
    assert_eq!(
        (log.compositors_built, log.senders_built, log.sent),
        (2, 1, 1)
    );
}

#[test]
fn a_device_lost_while_building_the_sender_drops_the_compositor_and_waits() {
    let max = MaxOut::new();
    let gpu = FakeGpu::default();
    gpu.script().sender.push_back(device_lost());
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&MaxJob::Black { stamp_100ns: 1 }, t0);
    assert_eq!(gpu.log().drops, ["compositor"]);
    assert!(!worker.holds_gpu());
    assert_eq!(max.status().device_resets, 1);
    worker.serve(&MaxJob::Black { stamp_100ns: 2 }, t0);
    assert_eq!(gpu.log().compositors_built, 1, "a build failure waits");
    worker.serve(&MaxJob::Black { stamp_100ns: 3 }, t0 + MAX_RETRY_BACKOFF);
    assert_eq!(gpu.log().sent, 1);
}

#[test]
fn a_refused_picture_or_a_lost_spout_frame_costs_one_boundary_only() {
    let max = MaxOut::new();
    let gpu = FakeGpu::default();
    gpu.script()
        .compose
        .push_back(GpuError::Picture(PictureError::Short { len: 1, need: 12 }));
    gpu.script().send.push_back(GpuError::Spout {
        call: "spout_sender_send",
        code: 3,
    });
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    for stamp in 1..=3 {
        worker.serve(&MaxJob::Black { stamp_100ns: stamp }, t0);
    }
    let log = gpu.log();
    assert_eq!(
        (log.compositors_built, log.senders_built),
        (1, 1),
        "nothing rebuilt"
    );
    assert!(log.drops.is_empty());
    assert_eq!(
        (log.drawn.len(), log.sent),
        (3, 1),
        "the picture refused, the frame lost, the third sent: no backoff"
    );
    drop(log);
    let status = max.status();
    assert_eq!(
        (
            status.failed,
            status.submitted,
            status.sender_backoffs,
            status.device_resets
        ),
        (2, 1, 0, 0)
    );
}

#[test]
fn an_unsupported_gpu_is_never_built_again() {
    let max = MaxOut::new();
    max.set_enabled(true);
    let gpu = FakeGpu::default();
    gpu.script().compositor.push_back(GpuError::Unsupported);
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&MaxJob::Black { stamp_100ns: 1 }, t0);
    worker.serve(
        &MaxJob::Black { stamp_100ns: 2 },
        t0 + Duration::from_secs(60),
    );
    assert_eq!(gpu.log().compositors_built, 1);
    let status = max.status();
    assert_eq!((status.state.as_str(), status.failed), ("unsupported", 0));
}

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

/// Off Windows the production GPU has no Direct3D: unsupported, once.
#[cfg(not(windows))]
#[test]
fn off_windows_the_production_gpu_is_unsupported() {
    use super::SpoutGpu;
    assert!(matches!(SpoutGpu.compositor(), Err(GpuError::Unsupported)));
    let max = MaxOut::new();
    let mut worker = MaxWorker::new(&max, SpoutGpu);
    worker.serve(&MaxJob::Black { stamp_100ns: 1 }, Instant::now());
    let status = max.status();
    assert_eq!((status.state.as_str(), status.failed), ("unsupported", 0));
    assert!(!worker.holds_gpu());
}

/// Poll `done` until it holds (bounded: 20 s).
pub(crate) fn wait_until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// `run_max_loop` on its own thread over `gpu`; joined by the caller.
pub(crate) fn spawn_loop(max: &Arc<MaxOut>, gpu: &FakeGpu) -> std::thread::JoinHandle<()> {
    let (max, gpu) = (max.clone(), gpu.clone());
    std::thread::Builder::new()
        .name("program-max-test".into())
        .spawn(move || run_max_loop(&max, gpu))
        .expect("spawn the loop")
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
