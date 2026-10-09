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
//!   its due instant, whatever the compose cost: a slot of the display
//!   refresh Arena renders in, when a [`VblankSource`] measures it
//!   (`program_max_vblank.rs`),
//!   else `MAX_SEND_LEAD` after the program offered it
//!   (`program_max_send.rs`).
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
//! #239: while the FHD Spout sender is wanted (`program_spout_fhd_enabled`
//! and MAX on, `MaxOut::fhd_wanted`), each job also goes out as
//! `SP-program`: a second compositor of 1920×1080 and a second sender, built
//! on this thread too, compose the same pictures at the same weight (the
//! picture the NDI `SP-program` carries), and the FHD sender sends right
//! after MAX at the same paced instant (alone at that instant when MAX's
//! boundary did not go out). Each output keeps its own objects, backoff,
//! lost-device state, telemetry and log ([`Side`]): a failure of one never
//! stops the other. Switched off, the FHD sender's objects are dropped at
//! the next boundary (Spout unregisters `SP-program`).
//!
//! The GPU is a trait ([`MaxGpu`]) so every decision here runs on Linux with
//! a fake; the production one is [`SpoutGpu`] (`sp-gpu`'s `Compositor` on
//! the picked adapter and its `SpoutSender`). Off Windows `sp-gpu` has no
//! Direct3D: its compositor reports `Unsupported`, and the worker then does
//! nothing more.

use std::time::{Duration, Instant};

use sp_gpu::{
    ComposeStats, Composition, FHD_HEIGHT, FHD_WIDTH, GpuError, Nv12Picture, SpoutSendStats,
    VblankGrid,
};
use tracing::{info, warn};

use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_max::{MaxJob, MaxNext, MaxOut, MaxPicture};
use crate::playback::program_max_send::{NoWait, SendClock, SendTiming, SpinClock, send_at};
use crate::playback::program_max_vblank::{
    Aligned, Due, VblankPacer, VblankSource, phase_after_vblank,
};
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
    /// #239: the size Spout's registry lists the sender at (what a receiver
    /// opens); `None` before its first send went out or while the registry
    /// cannot be read.
    fn listed_size(&self) -> Option<(u32, u32)>;
}

/// What builds the compositors and their senders, on the `program-max`
/// thread: MAX's, and (#239) the `SP-program` sender's.
pub trait MaxGpu {
    type Compositor: MaxCompositor;
    type Sender: MaxSender;
    fn compositor(&mut self) -> Result<Self::Compositor, GpuError>;
    fn sender(&mut self, compositor: &Self::Compositor) -> Result<Self::Sender, GpuError>;
    /// #239: the 1920×1080 compositor of the FHD Spout sender.
    fn fhd_compositor(&mut self) -> Result<Self::Compositor, GpuError>;
    /// #239: the sender `SP-program` on it.
    fn fhd_sender(&mut self, compositor: &Self::Compositor) -> Result<Self::Sender, GpuError>;
}

/// The production GPU: `sp_gpu::Compositor::new` (the largest hardware
/// adapter, never WARP) and the sender `SP-program-MAX`; #239: a 1920×1080
/// compositor on the same adapter and the sender `SP-program`.
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

    fn fhd_compositor(&mut self) -> Result<sp_gpu::Compositor, GpuError> {
        sp_gpu::Compositor::with_size(FHD_WIDTH, FHD_HEIGHT)
    }

    /// `mutants::skip`: as `sender`.
    #[cfg_attr(test, mutants::skip)]
    fn fhd_sender(
        &mut self,
        compositor: &sp_gpu::Compositor,
    ) -> Result<sp_gpu::SpoutSender, GpuError> {
        sp_gpu::SpoutSender::new_fhd(compositor)
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

    /// Spout's registry entry of the sender's name, read as a receiver
    /// does. `mutants::skip`: as `compose`.
    #[cfg_attr(test, mutants::skip)]
    fn listed_size(&self) -> Option<(u32, u32)> {
        let info = sp_gpu::spout_sender_info(self.name()).ok()??;
        Some((info.width, info.height))
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

    /// #239: the FHD sender's build failed (MAX's platform decides
    /// `Unsupported`, so any error here is a failed build).
    fn fhd_build(error: GpuError) -> Self {
        Skip::Failed(Failure::Build(error))
    }

    fn frame(error: GpuError) -> Self {
        Skip::Failed(Failure::Frame(error))
    }
}

/// An output with no compositor after its build step: a worker bug,
/// reported as a failed boundary, never a panic.
const NO_COMPOSITOR: GpuError = GpuError::NoObject {
    call: "the program-max compositor",
};

/// An output with no sender after its build step (see [`NO_COMPOSITOR`]).
const NO_SENDER: GpuError = GpuError::NoObject {
    call: "the program-max Spout sender",
};

/// The thread's outputs (#239).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Output {
    /// `SP-program-MAX`, 3840×2160.
    Max,
    /// `SP-program`, 1920×1080.
    Fhd,
}

/// What a failure did to an output's objects ([`Side::recover`]).
#[derive(Debug, PartialEq, Eq)]
enum Recovery {
    /// The device is gone: the compositor and the sender were dropped.
    DeviceLost,
    /// The sender is refused for good: it was dropped.
    SenderRefused,
    /// Nothing was dropped (a failed build of something else, one lost
    /// frame, a refused picture).
    Other,
}

/// One output's GPU objects and their recovery: MAX's, or (#239) the FHD
/// sender's. Each output keeps its own backoff and lost-device state, so a
/// failure of one never stops the other.
struct Side<C, S> {
    // Dropped first (declaration order): the sender holds references into
    // the compositor's device and render target.
    sender: Option<S>,
    compositor: Option<C>,
    /// No build before this instant.
    retry_at: Option<Instant>,
    /// A device was lost and no boundary of this output went out since.
    lost_unsent: bool,
}

impl<C, S> Side<C, S> {
    fn new() -> Self {
        Self {
            sender: None,
            compositor: None,
            retry_at: None,
            lost_unsent: false,
        }
    }

    fn holds(&self) -> bool {
        self.compositor.is_some() || self.sender.is_some()
    }

    /// Drop the sender (Spout unregisters its name), then the compositor.
    fn drop_gpu(&mut self) {
        self.sender = None;
        self.compositor = None;
    }

    /// After `failure`: a lost device drops both objects (rebuilt on the
    /// next job), a refused sender drops the sender; a build failure, a
    /// refusal or a second lost device before a boundary went out waits
    /// [`MAX_RETRY_BACKOFF`] before the next build. Returns what it dropped,
    /// for the caller's telemetry.
    fn recover(&mut self, failure: &Failure, now: Instant) -> Recovery {
        let error = failure.error();
        let refused = is_refusal(error);
        let lost = error.is_device_lost();
        let lost_again = lost && self.lost_unsent;
        let recovery = if lost {
            self.drop_gpu();
            self.lost_unsent = true;
            Recovery::DeviceLost
        } else if refused {
            self.sender = None;
            Recovery::SenderRefused
        } else {
            Recovery::Other
        };
        if matches!(failure, Failure::Build(_)) || refused || lost_again {
            self.retry_at = Some(now + MAX_RETRY_BACKOFF);
        }
        recovery
    }
}

/// The log lines one boundary wrote ([`MaxWorker::serve_lines`]).
#[derive(Debug, PartialEq, Eq)]
pub struct Lines {
    /// `SP-program-MAX`'s.
    pub max: Option<LogLine>,
    /// #239: the FHD sender's (`None` too while it is not wanted).
    pub fhd: Option<LogLine>,
}

/// The `program-max` thread's state: the GPU objects it built, the picture
/// ids and the backoffs. It reports every boundary to `out`.
pub struct MaxWorker<'a, G: MaxGpu> {
    out: &'a MaxOut,
    gpu: G,
    /// MAX's objects (dropped before the FHD sender's: declaration order).
    max: Side<G::Compositor, G::Sender>,
    /// #239: the `SP-program` sender's objects, built only while it is
    /// wanted.
    fhd: Side<G::Compositor, G::Sender>,
    ids: PictureIds,
    /// The GPU said `Unsupported`: never build again.
    unsupported: bool,
    /// Why MAX's boundaries do not go out now (`None`: they do).
    failing: Option<String>,
    /// #239: why the FHD sender's boundaries do not go out now.
    fhd_failing: Option<String>,
    /// Where the log's time starts.
    started: Instant,
    log: LogGate,
    fhd_log: LogGate,
    /// What the send waits on: [`NoWait`] unless [`with_clock`](Self::with_clock)
    /// gives another (the loop gives [`SpinClock`]).
    clock: Box<dyn SendClock>,
    /// The display refresh, when measured ([`with_vblank`](Self::with_vblank)).
    vblank: Option<Box<dyn VblankSource>>,
    pacer: VblankPacer,
}

/// A sent boundary's costs and timing ([`MaxWorker::attempt`]).
type Sent = (ComposeStats, SpoutSendStats, SendTiming, Option<Aligned>);

/// #239: a sent FHD boundary's costs.
type FhdSent = (ComposeStats, SpoutSendStats);

/// What one job did on each output: MAX's boundary, and the FHD sender's
/// (`None` while it is not wanted, or the platform is unsupported).
type Went = (Result<Sent, Skip>, Option<Result<FhdSent, Skip>>);

impl<'a, G: MaxGpu> MaxWorker<'a, G> {
    pub fn new(out: &'a MaxOut, gpu: G) -> Self {
        Self {
            out,
            gpu,
            max: Side::new(),
            fhd: Side::new(),
            ids: PictureIds::default(),
            unsupported: false,
            failing: None,
            fhd_failing: None,
            started: Instant::now(),
            log: LogGate::default(),
            fhd_log: LogGate::default(),
            clock: Box::new(NoWait),
            vblank: None,
            pacer: VblankPacer::default(),
        }
    }

    /// Send on `clock` (production: [`SpinClock`]).
    pub fn with_clock(mut self, clock: Box<dyn SendClock>) -> Self {
        self.clock = clock;
        self
    }

    /// Send on the refresh grid of `source` (production:
    /// `sp_gpu::VblankTracker`, the primary display's), and name its output.
    pub fn with_vblank(mut self, source: Box<dyn VblankSource>) -> Self {
        self.out.record_vblank_output(source.output());
        self.vblank = Some(source);
        self
    }

    /// Whether it holds a compositor or a sender, MAX's or the FHD
    /// sender's (an off setting makes it drop them).
    pub fn holds_gpu(&self) -> bool {
        self.max.holds() || self.fhd.holds()
    }

    /// [`serve_offered`](Self::serve_offered) a job offered at `now`.
    pub fn serve(&mut self, job: &MaxJob, now: Instant) -> Option<LogLine> {
        self.serve_offered(job, now, now)
    }

    /// Compose and send one boundary the program offered at `offered`, at
    /// `now`, report it, and log what changed ([`LogGate`]); returns the
    /// line MAX's log wrote ([`serve_lines`](Self::serve_lines)).
    pub fn serve_offered(
        &mut self,
        job: &MaxJob,
        offered: Instant,
        now: Instant,
    ) -> Option<LogLine> {
        self.serve_lines(job, offered, now).max
    }

    /// Compose and send one boundary the program offered at `offered`, at
    /// `now`, on MAX and (#239) on the FHD sender while it is wanted; report
    /// each and log what changed on each ([`LogGate`]); returns the lines
    /// logged. An FHD sender no longer wanted is dropped first.
    pub fn serve_lines(&mut self, job: &MaxJob, offered: Instant, now: Instant) -> Lines {
        let fhd_wanted = self.out.fhd_wanted();
        if !fhd_wanted && self.fhd.holds() {
            self.release_fhd();
        }
        let (max, fhd) = self.attempt(job, offered, now, fhd_wanted);
        let max = self.finish_max(max, job, now);
        let fhd = fhd.and_then(|went| self.finish_fhd(went, job, now));
        Lines { max, fhd }
    }

    /// Report MAX's boundary, recover from its failure, and log what
    /// changed; returns the line it logged.
    fn finish_max(
        &mut self,
        went: Result<Sent, Skip>,
        job: &MaxJob,
        now: Instant,
    ) -> Option<LogLine> {
        match went {
            Ok((compose, send, timing, aligned)) => {
                self.max.lost_unsent = false;
                self.failing = None;
                self.out.record_sent(compose, send);
                self.out.record_send_timing(timing);
                self.out.record_vblank(aligned);
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
                match self.max.recover(&failure, now) {
                    Recovery::DeviceLost => {
                        // No decoded frame stays pinned for a compositor
                        // that is gone.
                        self.ids.forget();
                        self.out.record_device_reset();
                    }
                    Recovery::SenderRefused => self.out.record_sender_backoff(),
                    Recovery::Other => {}
                }
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

    /// #239: report the FHD sender's boundary, recover from its failure,
    /// read where Spout lists it after its first boundary, and log what
    /// changed; returns the line it logged.
    fn finish_fhd(
        &mut self,
        went: Result<FhdSent, Skip>,
        job: &MaxJob,
        now: Instant,
    ) -> Option<LogLine> {
        match went {
            Ok((compose, send)) => {
                self.fhd.lost_unsent = false;
                self.fhd_failing = None;
                self.out.record_fhd_sent(compose, send);
                if self.out.fhd_listed().is_none() {
                    let listed = self.fhd.sender.as_ref().and_then(|s| s.listed_size());
                    self.out.record_fhd_listed(listed);
                }
            }
            Err(Skip::Backoff | Skip::Unsupported) => self.out.record_fhd_skipped(),
            Err(Skip::Failed(failure)) => {
                let why = failure.error().to_string();
                self.out.record_fhd_failed(&why);
                self.fhd_failing = Some(why);
                if self.fhd.recover(&failure, now) == Recovery::SenderRefused {
                    self.out.record_fhd_sender_backoff();
                }
                if self.fhd.sender.is_none() {
                    self.out.record_fhd_listed(None);
                }
            }
        }
        let at = elapsed_100ns(self.started, now);
        let line = self.fhd_log.observe(at, self.fhd_failing.as_deref());
        match &line {
            Some(LogLine::Failing { held_back }) => warn!(
                error = self.fhd_failing.as_deref().unwrap_or_default(),
                held_back = *held_back,
                stamp_100ns = job.stamp_100ns(),
                "program max: the SP-program (1920x1080) Spout boundaries do not go out"
            ),
            Some(LogLine::Recovered { held_back }) => info!(
                held_back = *held_back,
                stamp_100ns = job.stamp_100ns(),
                "program max: the SP-program (1920x1080) Spout boundaries go out"
            ),
            None => {}
        }
        line
    }

    /// Build what is missing (unless a backoff runs), compose `job` on each
    /// output, then send it when it is due: in the slot [`VblankPacer`]
    /// picks on the refresh grid (with the phase setting), else at the
    /// constant lead (`program_max_send::send_at`); MAX first, then (#239)
    /// the FHD sender at the same instant.
    fn attempt(&mut self, job: &MaxJob, offered: Instant, now: Instant, fhd_wanted: bool) -> Went {
        if self.unsupported {
            return (Err(Skip::Unsupported), None);
        }
        let max_built = self.build(Output::Max, now);
        if let Err(Skip::Unsupported) = max_built {
            return (Err(Skip::Unsupported), None);
        }
        let fhd_built = fhd_wanted.then(|| self.build(Output::Fhd, now));
        let ready = max_built.is_ok() || matches!(fhd_built, Some(Ok(())));
        // The pictures are labelled only for a boundary that is drawn.
        let composition = if ready {
            self.ids.composition(job)
        } else {
            Composition::Black
        };
        let max_composed = max_built.and_then(|()| self.compose(Output::Max, &composition));
        let fhd_composed =
            fhd_built.map(|built| built.and_then(|()| self.compose(Output::Fhd, &composition)));
        let mut paced = None;
        let max = match max_composed {
            Ok(compose) => {
                let pace = self.pace(offered);
                paced = Some(pace);
                self.send_max(compose, pace, offered)
            }
            Err(skip) => Err(skip),
        };
        let fhd = match fhd_composed {
            Some(Ok(compose)) => {
                let (due, _) = paced.unwrap_or_else(|| self.pace(offered));
                Some(self.send_fhd(compose, due.at, offered))
            }
            Some(Err(skip)) => Some(Err(skip)),
            None => None,
        };
        (max, fhd)
    }

    /// Build what `output` misses, unless its backoff runs: its compositor
    /// (MAX's records its adapter), then its sender.
    fn build(&mut self, output: Output, now: Instant) -> Result<(), Skip> {
        let side = match output {
            Output::Max => &mut self.max,
            Output::Fhd => &mut self.fhd,
        };
        if side.retry_at.is_some_and(|at| now < at) {
            return Err(Skip::Backoff);
        }
        let compositor = match side.compositor.take() {
            Some(compositor) => compositor,
            None => match output {
                Output::Max => {
                    let built = self.gpu.compositor().map_err(Skip::build)?;
                    self.out.record_adapter(built.adapter());
                    built
                }
                Output::Fhd => self.gpu.fhd_compositor().map_err(Skip::fhd_build)?,
            },
        };
        let compositor = side.compositor.insert(compositor);
        if side.sender.is_none() {
            let sender = match output {
                Output::Max => self.gpu.sender(compositor).map_err(Skip::build)?,
                Output::Fhd => self.gpu.fhd_sender(compositor).map_err(Skip::fhd_build)?,
            };
            side.sender = Some(sender);
        }
        Ok(())
    }

    /// Draw `composition` on `output`'s compositor (built by
    /// [`build`](Self::build)).
    fn compose(
        &mut self,
        output: Output,
        composition: &Composition<'_>,
    ) -> Result<ComposeStats, Skip> {
        let side = match output {
            Output::Max => &mut self.max,
            Output::Fhd => &mut self.fhd,
        };
        let Some(compositor) = side.compositor.as_mut() else {
            return Err(Skip::frame(NO_COMPOSITOR));
        };
        compositor.compose(composition).map_err(Skip::frame)
    }

    /// When the boundary offered at `offered` is due: [`VblankPacer`]'s slot
    /// on the refresh grid, else the constant lead; and that grid.
    fn pace(&mut self, offered: Instant) -> (Due, Option<VblankGrid>) {
        let clock = self.clock.as_mut();
        let grid = self.vblank.as_ref().and_then(|v| v.grid(clock.now()));
        let due = self.pacer.due(offered, grid, self.out.vblank_phase());
        (due, grid)
    }

    /// Send MAX's composed boundary at its due instant, and how it was
    /// paced on the grid.
    fn send_max(
        &mut self,
        compose: ComposeStats,
        (due, grid): (Due, Option<VblankGrid>),
        offered: Instant,
    ) -> Result<Sent, Skip> {
        let Some(sender) = self.max.sender.as_mut() else {
            return Err(Skip::frame(NO_SENDER));
        };
        let (send, timing) =
            send_at(self.clock.as_mut(), due.at, offered, || sender.send()).map_err(Skip::frame)?;
        let aligned = grid.map(|grid| Aligned {
            period: grid.period,
            phase_us: u64::try_from(phase_after_vblank(&grid, timing.started).as_micros())
                .unwrap_or(u64::MAX),
            repicked: due.repicked,
        });
        Ok((compose, send, timing, aligned))
    }

    /// #239: send the FHD sender's composed boundary at `due`: right after
    /// MAX's, whose send already waited for it (or alone, when MAX's
    /// boundary did not go out).
    fn send_fhd(
        &mut self,
        compose: ComposeStats,
        due: Instant,
        offered: Instant,
    ) -> Result<FhdSent, Skip> {
        let Some(sender) = self.fhd.sender.as_mut() else {
            return Err(Skip::frame(NO_SENDER));
        };
        let (send, _) =
            send_at(self.clock.as_mut(), due, offered, || sender.send()).map_err(Skip::frame)?;
        Ok((compose, send))
    }

    /// MAX is off, or the thread stops: drop the senders (Spout unregisters
    /// the names), then the compositors, and forget any backoff.
    pub fn release(&mut self) {
        if self.max.holds() {
            info!("program max: the Spout sender and the compositor are released");
        }
        self.max.drop_gpu();
        self.max.retry_at = None;
        self.ids.forget();
        self.release_fhd();
    }

    /// #239: drop the FHD sender's objects (Spout unregisters `SP-program`)
    /// and forget its backoff: it was switched off, MAX is off, or the
    /// thread stops.
    fn release_fhd(&mut self) {
        if self.fhd.holds() {
            info!("program max: the SP-program Spout sender and its compositor are released");
        }
        self.fhd.drop_gpu();
        self.fhd.retry_at = None;
        self.out.record_fhd_listed(None);
    }
}

/// The `program-max` thread: take each job from `out` and serve it on
/// `gpu`, each sent at its due instant on the real clock ([`SpinClock`]) —
/// on `vblank`'s refresh grid when one is given —, release the GPU while
/// MAX is off, exit on stop.
pub fn run_max_loop<G: MaxGpu>(out: &MaxOut, gpu: G, vblank: Option<Box<dyn VblankSource>>) {
    let _consumer = out.attach();
    let mut worker = MaxWorker::new(out, gpu).with_clock(Box::new(SpinClock));
    if let Some(source) = vblank {
        worker = worker.with_vblank(source);
    }
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

#[cfg(test)]
#[path = "program_max_worker_tests_fhd.rs"]
mod tests_fhd;
