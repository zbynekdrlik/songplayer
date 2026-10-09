//! The `program-max` thread (#223 S2, revision 3 R3-2): compose each
//! `SP-program-MAX` boundary on the GPU and send it over Spout.
//!
//! [`run_max_loop`] takes the [`MaxJob`]s the `SP-program` sender offers
//! (`program_max.rs`) and hands each to a [`MaxWorker`], which:
//!
//! - builds the compositor and then the Spout sender ON THIS THREAD, at the
//!   first job (neither is `Send`: they drive one Direct3D 11 immediate
//!   context);
//! - labels each native picture with an id ([`PictureIds`]), so a picture
//!   the compositor already holds (a held, paused or repeated frame: the
//!   same allocation as the last boundary's) is not uploaded again into
//!   its slot (a fade's incoming picture moves to the outgoing slot when the
//!   fade ends, and is uploaded there once);
//! - composes the boundary into the 3840×2160 render target and sends it at
//!   a constant phase: `MAX_SEND_LEAD` after the program offered it, whatever
//!   the compose cost (`program_max_send.rs`, the wall's 60 Hz render).
//!
//! Failures never panic the thread and never reach the program:
//!
//! - a lost device drops the sender and the compositor; both are rebuilt on
//!   the next job (`device_resets`), unless the rebuilt pair is lost again
//!   before a boundary went out: then the rebuild waits [`MAX_RETRY_BACKOFF`]
//!   (a GPU that keeps losing its device is never rebuilt 30 times a
//!   second);
//! - a refused sender (another sender holds `SP-program-MAX`, Spout did not
//!   list or register it) is dropped, and a new one is made only after
//!   [`MAX_RETRY_BACKOFF`] (`sender_backoffs`): an immediate retry would be
//!   refused the same way, and each makes a new 4K shared texture;
//! - a failed build (no hardware adapter, a shader that does not compile)
//!   is retried after the same backoff, never on every boundary;
//! - any other failure (a picture that is not whole NV12, one frame Spout
//!   lost) costs that boundary only.
//!
//! The log says what the thread does ([`LogGate`]): a WARN when boundaries
//! stop going out or fail for another reason, an INFO when they go out
//! again, at most one line per [`MAX_LOG_EVERY_100NS`]. A change inside that
//! window is not lost: the first boundary after it writes the state as it
//! is then, with how many boundaries were held back, so a failure that
//! alternates with sent boundaries never floods the log, and once a boundary
//! comes after the window the log's last line names the current state (a
//! switch-off or a stop inside the window can leave it one change behind;
//! the telemetry's `state` is always current).
//!
//! The GPU is a trait ([`MaxGpu`]) so every decision here runs on Linux with
//! a fake; the production one is [`SpoutGpu`] (`sp-gpu`'s `Compositor` on
//! the picked adapter and its `SpoutSender`). Off Windows `sp-gpu` has no
//! Direct3D: its compositor reports `Unsupported`, and the worker then does
//! nothing more.

use std::time::{Duration, Instant};

use sp_gpu::{ComposeStats, Composition, GpuError, Nv12Picture, SpoutSendStats};
use tracing::{info, warn};

use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_max::{MaxJob, MaxNext, MaxOut, MaxPicture};
use crate::playback::program_max_send::{NoWait, SendClock, SendTiming, SpinClock, send_paced};
use crate::playback::stat_window::WarnLimiter;

/// How long the thread waits before it builds again after a refused sender,
/// a failed build or a second lost device in a row ("a few seconds", the
/// S1b carry-over).
pub const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(3);

/// At most one log line of the thread's state (a failure WARN or a
/// recovery INFO) per 5 s of its time (100 ns).
pub const MAX_LOG_EVERY_100NS: i64 = 50_000_000;

/// The thread's time at `now` since `since`, in 100 ns (the unit of
/// `WarnLimiter`); 0 before `since`.
pub fn elapsed_100ns(since: Instant, now: Instant) -> i64 {
    let ns = now.saturating_duration_since(since).as_nanos();
    i64::try_from(ns / 100).unwrap_or(i64::MAX)
}

/// The line the log writes for one boundary ([`LogGate::observe`]).
#[derive(Debug, PartialEq, Eq)]
pub enum LogLine {
    /// Boundaries do not go out (a WARN with the reason).
    Failing { held_back: u64 },
    /// Boundaries go out again (an INFO).
    Recovered { held_back: u64 },
}

/// What the log last said about the thread, and its rate limit. Pure: the
/// worker logs the line it returns.
#[derive(Debug, Default)]
pub struct LogGate {
    limiter: WarnLimiter,
    /// What the log last said: `None` = boundaries go out (also the start),
    /// `Some(why)` = they do not, because of `why`.
    logged: Option<String>,
}

impl LogGate {
    /// The state after a boundary at `at_100ns` (`failing` = why boundaries
    /// do not go out, `None` = they do). A line when the state differs from
    /// what the log last said, at most one per [`MAX_LOG_EVERY_100NS`]; a
    /// difference held back is written by the first boundary after the
    /// window, as the state is then (`held_back` = the boundaries held
    /// back since the last line).
    pub fn observe(&mut self, at_100ns: i64, failing: Option<&str>) -> Option<LogLine> {
        if self.logged.as_deref() == failing {
            return None;
        }
        let held_back = self.limiter.admit(at_100ns, MAX_LOG_EVERY_100NS)?;
        self.logged = failing.map(str::to_owned);
        Some(match failing {
            Some(_) => LogLine::Failing { held_back },
            None => LogLine::Recovered { held_back },
        })
    }
}

/// The compositor half of the GPU: draw one boundary into the render target.
pub trait MaxCompositor {
    fn compose(&mut self, composition: &Composition<'_>) -> Result<ComposeStats, GpuError>;
    /// The adapter it runs on (the telemetry's `adapter`).
    fn adapter(&self) -> String;
}

/// The Spout half: share the render target as it is now.
pub trait MaxSender {
    fn send(&mut self) -> Result<SpoutSendStats, GpuError>;
}

/// What builds the compositor and its sender, on the `program-max` thread.
pub trait MaxGpu {
    type Compositor: MaxCompositor;
    type Sender: MaxSender;
    fn compositor(&mut self) -> Result<Self::Compositor, GpuError>;
    fn sender(&mut self, compositor: &Self::Compositor) -> Result<Self::Sender, GpuError>;
}

/// The production GPU: `sp_gpu::Compositor::new` (the largest hardware
/// adapter, never WARP) and the sender `SP-program-MAX`.
#[derive(Clone, Copy, Debug, Default)]
pub struct SpoutGpu;

impl MaxGpu for SpoutGpu {
    type Compositor = sp_gpu::Compositor;
    type Sender = sp_gpu::SpoutSender;

    fn compositor(&mut self) -> Result<sp_gpu::Compositor, GpuError> {
        sp_gpu::Compositor::new()
    }

    /// `mutants::skip`: it needs a compositor, which off Windows (the
    /// mutation runner) cannot exist; the WARP test drives the real sender.
    #[cfg_attr(test, mutants::skip)]
    fn sender(&mut self, compositor: &sp_gpu::Compositor) -> Result<sp_gpu::SpoutSender, GpuError> {
        sp_gpu::SpoutSender::new(compositor)
    }
}

impl MaxCompositor for sp_gpu::Compositor {
    /// `mutants::skip`: one call; off Windows no compositor exists to call
    /// it on (`sp-gpu`'s stub is uninhabited).
    #[cfg_attr(test, mutants::skip)]
    fn compose(&mut self, composition: &Composition<'_>) -> Result<ComposeStats, GpuError> {
        sp_gpu::Compositor::compose(self, composition)
    }

    /// `mutants::skip`: as `compose`.
    #[cfg_attr(test, mutants::skip)]
    fn adapter(&self) -> String {
        sp_gpu::Compositor::adapter(self).name.clone()
    }
}

impl MaxSender for sp_gpu::SpoutSender {
    /// `mutants::skip`: as `compose`.
    #[cfg_attr(test, mutants::skip)]
    fn send(&mut self) -> Result<SpoutSendStats, GpuError> {
        sp_gpu::SpoutSender::send(self)
    }
}

/// The ids of the pictures the compositor is given. `sp-gpu` skips the
/// upload of a picture whose id its slot already holds, so an id must never
/// name other bytes: ids come from a counter, never from an address (a
/// freed buffer's address comes back). A picture that is the SAME
/// allocation as one of the last composed boundary's keeps that one's id;
/// those allocations are held here, so none of them can be freed and
/// reused while it is compared.
#[derive(Debug, Default)]
pub struct PictureIds {
    next: u64,
    held: Vec<(SharedFrame, u64)>,
}

impl PictureIds {
    /// What `job` shows, its pictures labelled; this job's pictures are the
    /// ones held from now on.
    pub fn composition<'a>(&mut self, job: &'a MaxJob) -> Composition<'a> {
        let mut labelled = Vec::with_capacity(2);
        let composition = match job {
            MaxJob::Black { .. } => Composition::Black,
            MaxJob::Picture { picture, .. } => {
                Composition::Picture(self.label(&mut labelled, picture))
            }
            MaxJob::Fade {
                from,
                to,
                weight_q8,
                ..
            } => Composition::Fade {
                from: from.as_ref().map(|p| self.label(&mut labelled, p)),
                to: to.as_ref().map(|p| self.label(&mut labelled, p)),
                weight_q8: *weight_q8,
            },
        };
        self.held = labelled;
        composition
    }

    /// Hold no picture any more (the compositor is gone): the frames are
    /// freed, and every picture gets a new id.
    pub fn forget(&mut self) {
        self.held.clear();
    }

    /// `picture` with its id: the id of the same allocation held or already
    /// labelled in this job, else the next one.
    fn label<'a>(
        &mut self,
        labelled: &mut Vec<(SharedFrame, u64)>,
        picture: &'a MaxPicture,
    ) -> Nv12Picture<'a> {
        let known = self
            .held
            .iter()
            .chain(labelled.iter())
            .find(|(frame, _)| frame.ptr_eq(&picture.video))
            .map(|&(_, id)| id);
        let id = match known {
            Some(id) => id,
            None => {
                self.next += 1;
                self.next
            }
        };
        labelled.push((picture.video.clone(), id));
        picture.nv12(id)
    }
}

/// Whether `error` refuses the Spout sender for good: drop it, and a new
/// one only after the backoff.
pub fn is_refusal(error: &GpuError) -> bool {
    matches!(
        error,
        GpuError::SpoutNotRegistered { .. } | GpuError::SpoutNameTaken { .. }
    )
}

/// A failure, and where it happened.
#[derive(Debug)]
enum Failure {
    /// Building the compositor or the sender.
    Build(GpuError),
    /// Composing or sending a boundary.
    Frame(GpuError),
}

impl Failure {
    fn error(&self) -> &GpuError {
        match self {
            Failure::Build(error) | Failure::Frame(error) => error,
        }
    }
}

/// Why a boundary did not go out.
#[derive(Debug)]
enum Skip {
    /// The thread waits out a backoff before it builds again.
    Backoff,
    /// No Direct3D / Spout here.
    Unsupported,
    Failed(Failure),
}

impl Skip {
    fn build(error: GpuError) -> Self {
        match error {
            GpuError::Unsupported => Skip::Unsupported,
            error => Skip::Failed(Failure::Build(error)),
        }
    }

    fn frame(error: GpuError) -> Self {
        Skip::Failed(Failure::Frame(error))
    }
}

/// The `program-max` thread's state: the GPU objects it built, the picture
/// ids and the backoff. It reports every boundary to `out`.
pub struct MaxWorker<'a, G: MaxGpu> {
    out: &'a MaxOut,
    gpu: G,
    // Dropped first (declaration order): the sender holds references into
    // the compositor's device and render target.
    sender: Option<G::Sender>,
    compositor: Option<G::Compositor>,
    ids: PictureIds,
    /// No build before this instant.
    retry_at: Option<Instant>,
    /// The GPU said `Unsupported`: never build again.
    unsupported: bool,
    /// A device was lost and no boundary went out since.
    lost_unsent: bool,
    /// Why boundaries do not go out now (`None`: they do).
    failing: Option<String>,
    /// Where the log's time starts.
    started: Instant,
    log: LogGate,
    /// What the send waits on: [`NoWait`] unless [`with_clock`](Self::with_clock)
    /// gives another (the loop gives [`SpinClock`]).
    clock: Box<dyn SendClock>,
}

impl<'a, G: MaxGpu> MaxWorker<'a, G> {
    pub fn new(out: &'a MaxOut, gpu: G) -> Self {
        Self {
            out,
            gpu,
            sender: None,
            compositor: None,
            ids: PictureIds::default(),
            retry_at: None,
            unsupported: false,
            lost_unsent: false,
            failing: None,
            started: Instant::now(),
            log: LogGate::default(),
            clock: Box::new(NoWait),
        }
    }

    /// Send on `clock` (production: [`SpinClock`]).
    pub fn with_clock(mut self, clock: Box<dyn SendClock>) -> Self {
        self.clock = clock;
        self
    }

    /// Whether it holds a compositor or a sender (an off setting makes it
    /// drop them).
    pub fn holds_gpu(&self) -> bool {
        self.compositor.is_some() || self.sender.is_some()
    }

    /// [`serve_offered`](Self::serve_offered) a job offered at `now`.
    pub fn serve(&mut self, job: &MaxJob, now: Instant) -> Option<LogLine> {
        self.serve_offered(job, now, now)
    }

    /// Compose and send one boundary the program offered at `offered`, at
    /// `now`, report it, and log what changed ([`LogGate`]); returns the
    /// line it logged.
    pub fn serve_offered(
        &mut self,
        job: &MaxJob,
        offered: Instant,
        now: Instant,
    ) -> Option<LogLine> {
        match self.attempt(job, offered, now) {
            Ok((compose, send, timing)) => {
                self.lost_unsent = false;
                self.failing = None;
                self.out.record_sent(compose, send);
                self.out.record_send_timing(timing);
            }
            Err(Skip::Backoff) => self.out.record_skipped(),
            Err(Skip::Unsupported) => {
                self.unsupported = true;
                self.out.record_unsupported();
                return None;
            }
            Err(Skip::Failed(failure)) => {
                let why = failure.error().to_string();
                self.out.record_failed(&why);
                self.failing = Some(why);
                self.recover(&failure, now);
            }
        }
        let at = elapsed_100ns(self.started, now);
        let line = self.log.observe(at, self.failing.as_deref());
        match &line {
            Some(LogLine::Failing { held_back }) => warn!(
                error = self.failing.as_deref().unwrap_or_default(),
                held_back = *held_back,
                stamp_100ns = job.stamp_100ns(),
                "program max: boundaries do not go out"
            ),
            Some(LogLine::Recovered { held_back }) => info!(
                held_back = *held_back,
                stamp_100ns = job.stamp_100ns(),
                "program max: boundaries go out"
            ),
            None => {}
        }
        line
    }

    /// Build what is missing (unless a backoff runs), then compose `job`
    /// and send it when it is due (`program_max_send::send_paced`).
    fn attempt(
        &mut self,
        job: &MaxJob,
        offered: Instant,
        now: Instant,
    ) -> Result<(ComposeStats, SpoutSendStats, SendTiming), Skip> {
        if self.unsupported {
            return Err(Skip::Unsupported);
        }
        if self.retry_at.is_some_and(|at| now < at) {
            return Err(Skip::Backoff);
        }
        let compositor = match self.compositor.take() {
            Some(compositor) => compositor,
            None => {
                let built = self.gpu.compositor().map_err(Skip::build)?;
                self.out.record_adapter(built.adapter());
                built
            }
        };
        let compositor = self.compositor.insert(compositor);
        let sender = match self.sender.take() {
            Some(sender) => sender,
            None => self.gpu.sender(compositor).map_err(Skip::build)?,
        };
        let sender = self.sender.insert(sender);
        let composition = self.ids.composition(job);
        let compose = compositor.compose(&composition).map_err(Skip::frame)?;
        let clock = self.clock.as_mut();
        let (send, timing) = send_paced(clock, offered, || sender.send()).map_err(Skip::frame)?;
        Ok((compose, send, timing))
    }

    /// After a failure: a lost device drops both objects (rebuilt on the
    /// next job), a refused sender drops the sender; a build failure, a
    /// refusal or a second lost device before a boundary went out waits
    /// [`MAX_RETRY_BACKOFF`] before the next build.
    fn recover(&mut self, failure: &Failure, now: Instant) {
        let error = failure.error();
        let refused = is_refusal(error);
        let lost_again = error.is_device_lost() && self.lost_unsent;
        if error.is_device_lost() {
            self.drop_gpu();
            self.out.record_device_reset();
            self.lost_unsent = true;
        } else if refused {
            self.sender = None;
            self.out.record_sender_backoff();
        }
        if matches!(failure, Failure::Build(_)) || refused || lost_again {
            self.retry_at = Some(now + MAX_RETRY_BACKOFF);
        }
    }

    /// MAX is off, or the thread stops: drop the sender (Spout unregisters
    /// the name), then the compositor, and forget any backoff.
    pub fn release(&mut self) {
        if self.holds_gpu() {
            info!("program max: the Spout sender and the compositor are released");
        }
        self.drop_gpu();
        self.retry_at = None;
    }

    fn drop_gpu(&mut self) {
        self.sender = None;
        self.compositor = None;
        self.ids.forget();
    }
}

/// The `program-max` thread: take each job from `out` and serve it on
/// `gpu`, each sent at its due instant on the real clock ([`SpinClock`]),
/// release the GPU while MAX is off, exit on stop.
pub fn run_max_loop<G: MaxGpu>(out: &MaxOut, gpu: G) {
    let _consumer = out.attach();
    let mut worker = MaxWorker::new(out, gpu).with_clock(Box::new(SpinClock));
    loop {
        match out.next(worker.holds_gpu()) {
            MaxNext::Job(job, offered) => {
                worker.serve_offered(&job, offered, Instant::now());
            }
            MaxNext::Release => worker.release(),
            MaxNext::Stop => break,
        }
    }
    worker.release();
    info!("program max: stopped");
}

#[cfg(test)]
#[path = "program_max_worker_tests.rs"]
pub(crate) mod tests;

#[cfg(test)]
#[path = "program_max_worker_tests_send.rs"]
mod tests_send;
