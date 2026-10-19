//! #223 S2: the `program-max` thread's decisions, on a fake GPU: what it
//! builds and when, the picture ids, what a lost device, a refused sender, a
//! failed build or a lost frame cost, the backoff, the release, and the loop.
//! The fake ([`FakeGpu`]) is `pub(crate)`: `program_output_tests_max.rs`
//! holds its compose to stall the consumer, and #239's
//! `program_max_worker_tests_fhd.rs` drives its FHD side. #251 step 2 +
//! #279 P1b: ONE fake compositor holds the FHD program as its primary
//! target and the MAX target the worker adds (`sp_gpu::Targets`, the
//! compositor's own book-keeping); one compose draws every target in draw
//! order, and each sender knows whether it is the FHD one's. It also runs
//! P1a's UYVY pass: a readback names the compose it came from.
//! Wired via `#[cfg(test)] #[path = "program_max_worker_tests.rs"] pub(crate) mod tests;`.

use std::collections::VecDeque;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use sp_core::config::SpoutSend;
use sp_gpu::{
    ComposeStats, Composition, FHD_TARGET, GpuError, MAX_TARGET, PictureError, Side,
    SpoutSendStats, Target, TargetId, Targets,
};

use super::{
    MAX_RETRY_BACKOFF, MaxCompositor, MaxGpu, MaxSender, MaxWorker, PictureIds, is_refusal,
    run_max_loop,
};
use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_max::{MaxJob, MaxOut, MaxPicture};

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
    /// #279 P2: a ring frame (`Side::Gpu`, no stride or bytes), not an
    /// uploaded picture.
    pub gpu: bool,
}

impl Pic {
    fn of(side: &Side<'_>) -> Self {
        match side {
            Side::Cpu(picture) => Self {
                id: picture.id,
                width: picture.width,
                height: picture.height,
                stride: picture.stride,
                len: picture.data.len(),
                gpu: false,
            },
            Side::Gpu(frame) => Self {
                id: frame.id(),
                width: frame.width(),
                height: frame.height(),
                stride: 0,
                len: 0,
                gpu: true,
            },
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
    /// "sender" / "fhd sender" / "compositor", in drop order.
    pub drops: Vec<&'static str>,
    /// What each compose drew into MAX's target (#279 P1b: while the
    /// compositor holds it).
    pub drawn: Vec<Drawn>,
    pub sent: u32,
    /// #251 step 2 + #279 P1b: the targets added to a compositor
    /// (`add_target`: MAX's) and how many were removed (`remove_target`).
    pub targets_added: Vec<Target>,
    pub targets_removed: u32,
    /// #239: the FHD senders built, what each compose drew into the FHD
    /// target (#279 P1b: the primary, every compose; the SAME compose as
    /// `drawn`'s), the FHD sends.
    pub fhd_senders_built: u32,
    pub fhd_drawn: Vec<Drawn>,
    pub fhd_sent: u32,
    /// "fhd sender built" / "fhd draw" / "max draw" (in the same compose,
    /// in draw order: the primary first) / "ndi read" / "max send" / "fhd
    /// send" / "max target removed" / "ndi on" / "ndi off", in order; and
    /// the registry reads.
    pub order: Vec<&'static str>,
    pub listed_reads: u32,
    /// #279 P1b: the composes made (a readback names its compose by it),
    /// the UYVY passes enabled, the readbacks that answered a picture.
    pub composes: u32,
    pub uyvy_enabled: u32,
    pub uyvy_read: u32,
    /// #251: each Spout hand-off a sender was given ("max" / "fhd").
    pub send_modes: Vec<(&'static str, SpoutSend)>,
}

/// The fake GPU's next results; an empty queue answers `Ok`.
#[derive(Default)]
pub(crate) struct Script {
    pub compositor: VecDeque<GpuError>,
    pub sender: VecDeque<GpuError>,
    pub compose: VecDeque<GpuError>,
    pub send: VecDeque<GpuError>,
    /// #251 step 2: what adding the FHD target answers.
    pub add_target: VecDeque<GpuError>,
    /// #239: the FHD sender's, and what each registry read answers (an
    /// empty queue: [`FAKE_LISTED`]).
    pub fhd_sender: VecDeque<GpuError>,
    pub fhd_send: VecDeque<GpuError>,
    pub listed: VecDeque<Option<(u32, u32)>>,
    /// #279 P1b: what enabling the UYVY pass and reading it back answer.
    pub enable_uyvy: VecDeque<GpuError>,
    pub read_uyvy: VecDeque<GpuError>,
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

/// The fake compositor's adapter.
pub(crate) const FAKE_ADAPTER: &str = "Fake GPU";

/// The stats every fake compose and send report.
pub(crate) const UPLOAD_US: u64 = 11;
pub(crate) const DRAW_US: u64 = 22;
pub(crate) const SEND_US: u64 = 33;

/// #239: the FHD target's own draw (its part of the compose's) and the FHD
/// send (their own, so the FHD telemetry is never MAX's).
pub(crate) const FHD_DRAW_US: u64 = 44;
pub(crate) const FHD_SEND_US: u64 = 55;

/// #239: where the fake registry lists a sender.
pub(crate) const FAKE_LISTED: (u32, u32) = (1920, 1080);

/// #279 P1b: the UYVY pass's own part of every fake compose that ran it.
pub(crate) const UYVY_US: u64 = 66;

/// #279 P1b: the bytes of a fake readback: `[n; FAKE_UYVY_BYTES]` for the
/// compositor's n-th compose.
pub(crate) const FAKE_UYVY_BYTES: usize = 8;

/// #279 P1b: the fake's readback of its `n`-th compose.
pub(crate) fn fake_uyvy(n: u32) -> Vec<u8> {
    vec![u8::try_from(n).unwrap_or(u8::MAX); FAKE_UYVY_BYTES]
}

/// The fake GPU: its compositor and senders record into [`Log`] and answer
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

/// The fake compositor: the FHD primary target and the targets added to it
/// (MAX's; the real `Targets` book-keeping mints their ids), and P1a's UYVY
/// pass (the target it reads, the composes since it was enabled).
pub(crate) struct FakeCompositor {
    shared: Arc<FakeShared>,
    targets: Targets<Target>,
    uyvy: Option<(TargetId, u32)>,
}

/// The second field: the sender is the FHD one's (#239).
pub(crate) struct FakeSender(Arc<FakeShared>, bool);

fn log_of(shared: &FakeShared) -> MutexGuard<'_, Log> {
    shared.log.lock().unwrap_or_else(|p| p.into_inner())
}

fn next_error(
    shared: &FakeShared,
    pick: impl Fn(&mut Script) -> Option<GpuError>,
) -> Option<GpuError> {
    let mut guard = shared.script.lock().unwrap_or_else(|p| p.into_inner());
    // A typed binding derefs the guard to the `Script` the picker takes
    // (`&mut *guard` trips clippy::explicit_auto_deref under -D warnings).
    let script: &mut Script = &mut guard;
    pick(script)
}

impl Drop for FakeCompositor {
    fn drop(&mut self) {
        log_of(&self.shared).drops.push("compositor");
    }
}

impl Drop for FakeSender {
    fn drop(&mut self) {
        let what = if self.1 { "fhd sender" } else { "sender" };
        log_of(&self.0).drops.push(what);
    }
}

impl MaxGpu for FakeGpu {
    type Compositor = FakeCompositor;
    type Sender = FakeSender;

    fn compositor(&mut self) -> Result<FakeCompositor, GpuError> {
        log_of(&self.0).compositors_built += 1;
        match next_error(&self.0, |s| s.compositor.pop_front()) {
            Some(error) => Err(error),
            None => Ok(FakeCompositor {
                shared: self.0.clone(),
                targets: Targets::new(FHD_TARGET),
                uyvy: None,
            }),
        }
    }

    fn sender(
        &mut self,
        compositor: &FakeCompositor,
        target: TargetId,
    ) -> Result<FakeSender, GpuError> {
        assert_eq!(
            compositor.targets.get(target),
            Some(&MAX_TARGET),
            "MAX's sender of the MAX target the compositor holds"
        );
        log_of(&self.0).senders_built += 1;
        match next_error(&self.0, |s| s.sender.pop_front()) {
            Some(error) => Err(error),
            None => Ok(FakeSender(self.0.clone(), false)),
        }
    }

    fn fhd_sender(
        &mut self,
        compositor: &FakeCompositor,
        target: TargetId,
    ) -> Result<FakeSender, GpuError> {
        assert_eq!(
            (target, compositor.targets.get(target)),
            (TargetId::PRIMARY, Some(&FHD_TARGET)),
            "the FHD sender of the compositor's primary, the FHD target"
        );
        let mut log = log_of(&self.0);
        log.fhd_senders_built += 1;
        log.order.push("fhd sender built");
        drop(log);
        match next_error(&self.0, |s| s.fhd_sender.pop_front()) {
            Some(error) => Err(error),
            None => Ok(FakeSender(self.0.clone(), true)),
        }
    }
}

/// What `composition` shows, as the fake records it.
fn drawn_of(composition: &Composition<'_>) -> Drawn {
    match composition {
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
    }
}

impl MaxCompositor for FakeCompositor {
    fn compose(&mut self, composition: &Composition<'_>) -> Result<ComposeStats, GpuError> {
        let hold = self
            .shared
            .hold
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(hold) = hold {
            let _ = hold.entered.send(());
            hold.gate.wait();
        }
        let drawn = drawn_of(composition);
        let mut log = log_of(&self.shared);
        log.composes += 1;
        for (id, _) in self.targets.iter() {
            if id == TargetId::PRIMARY {
                log.fhd_drawn.push(drawn.clone());
                log.order.push("fhd draw");
            } else {
                log.drawn.push(drawn.clone());
                log.order.push("max draw");
            }
        }
        drop(log);
        match next_error(&self.shared, |s| s.compose.pop_front()) {
            Some(error) => Err(error),
            None => {
                if let Some((_, composes)) = self.uyvy.as_mut() {
                    *composes += 1;
                }
                let stats = super::tests_upload::fake_stats(composition);
                Ok(ComposeStats {
                    uyvy_us: self.uyvy.map(|_| UYVY_US),
                    ..stats
                })
            }
        }
    }

    fn adapter(&self) -> String {
        FAKE_ADAPTER.to_string()
    }

    fn add_target(&mut self, target: Target) -> Result<TargetId, GpuError> {
        log_of(&self.shared).targets_added.push(target);
        match next_error(&self.shared, |s| s.add_target.pop_front()) {
            Some(error) => Err(error),
            None => Ok(self.targets.add(target)),
        }
    }

    fn remove_target(&mut self, target: TargetId) {
        assert!(
            self.targets.remove(target).is_some(),
            "a target the compositor holds is removed"
        );
        let mut log = log_of(&self.shared);
        log.targets_removed += 1;
        log.order.push("max target removed");
    }

    /// Each target its own figure (the FHD primary `FHD_DRAW_US`, an added
    /// one `DRAW_US`), so a worker reading the wrong target's share fails.
    fn draw_us(&self, target: TargetId) -> u64 {
        match self.targets.get(target) {
            Some(_) if target == TargetId::PRIMARY => FHD_DRAW_US,
            Some(_) => DRAW_US,
            None => 0,
        }
    }

    fn enable_uyvy(&mut self, target: TargetId) -> Result<(), GpuError> {
        assert!(
            self.targets.get(target).is_some(),
            "a pass of a target the compositor holds"
        );
        let mut log = log_of(&self.shared);
        log.order.push("ndi on");
        drop(log);
        if let Some(error) = next_error(&self.shared, |s| s.enable_uyvy.pop_front()) {
            return Err(error);
        }
        log_of(&self.shared).uyvy_enabled += 1;
        self.uyvy = Some((target, 0));
        Ok(())
    }

    fn disable_uyvy(&mut self) {
        log_of(&self.shared).order.push("ndi off");
        self.uyvy = None;
    }

    /// The pass's newest copy: `fake_uyvy(n)` of the compositor's n-th
    /// compose (`Ok(false)` before a compose since the enable).
    fn read_uyvy(&mut self, out: &mut Vec<u8>) -> Result<bool, GpuError> {
        if let Some(error) = next_error(&self.shared, |s| s.read_uyvy.pop_front()) {
            return Err(error);
        }
        let Some((_, composes)) = self.uyvy else {
            return Err(GpuError::NoObject { call: "read_uyvy" });
        };
        if composes == 0 {
            return Ok(false);
        }
        let mut log = log_of(&self.shared);
        log.order.push("ndi read");
        log.uyvy_read += 1;
        out.clear();
        out.extend_from_slice(&fake_uyvy(log.composes));
        Ok(true)
    }
}

impl MaxSender for FakeSender {
    fn set_send_mode(&mut self, mode: SpoutSend) {
        let which = if self.1 { "fhd" } else { "max" };
        log_of(&self.0).send_modes.push((which, mode));
    }

    fn send(&mut self) -> Result<SpoutSendStats, GpuError> {
        let fhd = self.1;
        let scripted = next_error(&self.0, |s| {
            if fhd {
                s.fhd_send.pop_front()
            } else {
                s.send.pop_front()
            }
        });
        if let Some(error) = scripted {
            return Err(error);
        }
        let mut log = log_of(&self.0);
        if fhd {
            log.fhd_sent += 1;
            log.order.push("fhd send");
            Ok(SpoutSendStats {
                send_us: FHD_SEND_US,
                relisted: false,
            })
        } else {
            log.sent += 1;
            log.order.push("max send");
            Ok(SpoutSendStats {
                send_us: SEND_US,
                relisted: false,
            })
        }
    }

    fn listed_size(&self) -> Option<(u32, u32)> {
        log_of(&self.0).listed_reads += 1;
        next_listed(&self.0)
    }
}

/// #239: the next scripted registry answer, else [`FAKE_LISTED`].
fn next_listed(shared: &FakeShared) -> Option<(u32, u32)> {
    let mut script = shared.script.lock().unwrap_or_else(|p| p.into_inner());
    script.listed.pop_front().unwrap_or(Some(FAKE_LISTED))
}

/// A `width`×`height` NV12 picture (stride = width) of its own allocation.
pub(crate) fn picture(width: u32, height: u32) -> MaxPicture {
    MaxPicture {
        width,
        height,
        stride: width,
        video: SharedFrame::new(vec![80; (width * height * 3 / 2) as usize]).into(),
    }
}

pub(crate) fn plain(stamp: i64, picture: &MaxPicture) -> MaxJob {
    MaxJob::Picture {
        stamp_100ns: stamp,
        picture: picture.clone(),
    }
}

pub(crate) fn pic(id: u64, picture: &MaxPicture) -> Pic {
    Pic {
        id,
        width: picture.width,
        height: picture.height,
        stride: picture.stride,
        len: picture.video.len(),
        gpu: false,
    }
}

pub(crate) fn device_lost() -> GpuError {
    GpuError::DeviceLost {
        call: "Present",
        hresult: 0x887A_0005,
    }
}

pub(crate) fn not_registered() -> GpuError {
    GpuError::SpoutNotRegistered {
        name: "SP-program-MAX".into(),
        why: "test",
        holder: None,
    }
}

/// The ids `composition` labelled, in draw order (`None` = a missing side).
fn ids(composition: &Composition<'_>) -> Vec<Option<u64>> {
    match composition {
        Composition::Black => Vec::new(),
        Composition::Picture(p) => vec![Some(p.id())],
        Composition::Fade { from, to, .. } => vec![from.map(|p| p.id()), to.map(|p| p.id())],
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
        video: SharedFrame::new(a.video.expect_cpu().to_vec()).into(),
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
    let Composition::Picture(Side::Cpu(p)) = labels.composition(&job) else {
        panic!("a CPU picture composes an uploaded picture");
    };
    assert_eq!(p.data.as_ptr(), a.video.expect_cpu().as_ptr());
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
    max.set_enabled(true);
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    assert!(!worker.holds_gpu(), "nothing built before a job");
    let a = picture(4, 2);
    let t0 = Instant::now();
    assert_eq!(
        worker.serve(&plain(1, &a), t0),
        None,
        "boundaries go out from the start: nothing to log"
    );
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
    assert_eq!(status.adapter.as_deref(), Some(FAKE_ADAPTER));
}

#[test]
fn a_second_lost_device_before_a_boundary_went_out_waits_the_backoff() {
    let max = MaxOut::new();
    max.set_enabled(true);
    let gpu = FakeGpu::default();
    gpu.script().compose.push_back(device_lost());
    gpu.script().compose.push_back(device_lost());
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&MaxJob::Black { stamp_100ns: 1 }, t0);
    worker.serve(&MaxJob::Black { stamp_100ns: 2 }, t0);
    assert_eq!(
        gpu.log().compositors_built,
        2,
        "the first loss rebuilds at once, and the rebuilt pair is lost again"
    );
    worker.serve(&MaxJob::Black { stamp_100ns: 3 }, t0);
    assert_eq!(
        gpu.log().compositors_built,
        2,
        "lost again before a boundary went out: no rebuild inside the backoff"
    );
    worker.serve(&MaxJob::Black { stamp_100ns: 4 }, t0 + MAX_RETRY_BACKOFF);
    let log = gpu.log();
    assert_eq!((log.compositors_built, log.sent), (3, 1));
    drop(log);
    assert_eq!(max.status().device_resets, 2);
}

#[test]
fn a_lost_device_after_a_sent_boundary_rebuilds_at_once_again() {
    let max = MaxOut::new();
    max.set_enabled(true);
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    gpu.script().compose.push_back(device_lost());
    worker.serve(&MaxJob::Black { stamp_100ns: 1 }, t0);
    worker.serve(&MaxJob::Black { stamp_100ns: 2 }, t0);
    assert_eq!(gpu.log().sent, 1, "rebuilt and sent");
    gpu.script().compose.push_back(device_lost());
    worker.serve(&MaxJob::Black { stamp_100ns: 3 }, t0);
    worker.serve(&MaxJob::Black { stamp_100ns: 4 }, t0);
    let log = gpu.log();
    assert_eq!(
        (log.compositors_built, log.sent),
        (3, 2),
        "a boundary went out between the two losses: the rebuild does not wait"
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
    max.set_enabled(true);
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
    max.set_enabled(true);
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
    max.set_enabled(true);
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
    max.set_enabled(true);
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
    max.set_enabled(true);
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
        .spawn(move || run_max_loop(&max, gpu, None, None))
        .expect("spawn the loop")
}

/// #279 P2: ring frames on MAX's side.
#[path = "program_max_worker_tests_gpu.rs"]
mod ring_frames;
