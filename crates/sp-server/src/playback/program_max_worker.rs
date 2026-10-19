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
//!   (`program_max_send.rs`); #251: on the refresh grid the compose itself
//!   waits for its own instant, a fixed phase of that grid (the setting
//!   `program_max_compose_phase_ms`), so its GPU work never drifts into the
//!   instants Arena renders and reads Spout in (off the grid: at once).
//!
//! #239 + #251 step 2 + #279 P1b: ONE compositor draws every output. Its
//! PRIMARY target is the FHD program (`FHD_TARGET`, 1920×1080), there for
//! its life; MAX's 3840×2160 target is ADDED with MAX's sender while MAX is
//! on (`MaxOut::enabled`) and taken off with it. While the FHD Spout
//! sender is wanted (`program_spout_fhd_enabled` and MAX on,
//! `Wants::fhd`) the sender `SP-program` sends the primary, the
//! picture the NDI `SP-program` carries. One compose at the compose instant
//! uploads each picture ONCE and draws every target; at the due instant
//! MAX's send runs, then the FHD one's: plain copies, the GPU work all done
//! at the quiet phase (#251 comment 6192123775). MAX goes first, whole
//! (#239): an FHD sender still to build (its Spout open) is built after
//! MAX's send and sends from the next boundary, unless MAX's boundary is not
//! drawn at all (then it is built first and goes out alone, at the due
//! instant). A device the FHD side loses after MAX's boundary went out is
//! MAX's too: the boundary stays sent, MAX's state and log name the loss.
//! #279 P1b: while NDI `SP-program` comes from the GPU (`Wants::ndi`)
//! the primary is read back as UYVY after each compose, before the sends
//! (`program_max_ndi.rs`); with MAX off the compositor keeps composing for
//! NDI alone.
//!
//! Failures never panic the thread and never reach the program:
//!
//! - a lost device drops both senders and the compositor (one device, one
//!   event: `device_resets`); all are rebuilt on the next job, unless the
//!   device is lost again before a boundary went out, or while building:
//!   then the rebuild waits [`MAX_RETRY_BACKOFF`] (a GPU that keeps losing
//!   its device is never rebuilt 30 times a second);
//! - a refused sender (another sender holds its name, Spout did not list or
//!   register it) is dropped, and a new one is made only after
//!   [`MAX_RETRY_BACKOFF`] (`sender_backoffs`, `fhd.sender_backoffs`): an
//!   immediate retry would be refused the same way, and each makes a new
//!   shared texture. Each output keeps its own backoff: a refused FHD
//!   sender never stops MAX, and the other way round;
//! - a failed build (no hardware adapter, a shader that does not compile, a
//!   sender or a target that could not be made) is retried after the same
//!   backoff, never on every boundary;
//! - any other failure (a picture that is not whole NV12, one frame Spout
//!   lost) costs that boundary only.
//!
//! The log says what the thread does ([`LogGate`], one per output): a WARN
//! when boundaries stop going out or fail for another reason, an INFO when
//! they go out again, at most one line per [`MAX_LOG_EVERY_100NS`]. A
//! change inside that window is not lost: the first boundary after it
//! writes the state as it is then, with how many boundaries were held back,
//! so a failure that alternates with sent boundaries never floods the log,
//! and once a boundary comes after the window the log's last line names the
//! current state (a switch-off or a stop inside the window can leave it one
//! change behind; the telemetry's `state` is always current).
//!
//! #243 follow-up: after each boundary the thread also reads who holds
//! Spout's sender list, names a program that keeps it and starts the
//! sentinel that wins it back (`program_max_list.rs`, [`MaxWorker::with_list`]);
//! a quiesce stops that sentinel too.
//!
//! The GPU is a trait ([`MaxGpu`], `program_max_gpu.rs`) so every decision
//! here runs on Linux with a fake; the production one is [`SpoutGpu`]. Off
//! Windows `sp-gpu` has no Direct3D: its compositor reports `Unsupported`,
//! and the worker then does nothing more.

use std::time::{Duration, Instant};

use sp_gpu::{ComposeStats, GpuError, MAX_TARGET, SpoutSendStats, Target, TargetId, VblankGrid};
use tracing::{info, warn};

use crate::playback::program_max::{MaxJob, MaxNext, MaxOut, Wants};
pub use crate::playback::program_max_gpu::{
    MaxCompositor, MaxGpu, MaxSender, SpoutGpu, is_refusal,
};
pub use crate::playback::program_max_ids::PictureIds;
use crate::playback::program_max_list::{ListSide, SpoutList};
use crate::playback::program_max_send::{
    NoWait, SendClock, SendTiming, SpinClock, send_at, wait_for,
};
use crate::playback::program_max_vblank::{
    Aligned, Due, VblankLog, VblankPacer, VblankSource, phase_after_vblank_us,
};
#[path = "program_max_log.rs"]
mod gate;
pub use gate::{LogGate, LogLine, MAX_LOG_EVERY_100NS, elapsed_100ns};
#[path = "program_max_ndi.rs"]
mod ndi;
pub use ndi::PassLine;
#[path = "program_max_release.rs"]
mod release;

/// How long the thread waits before it builds again after a refused sender,
/// a failed build or a second lost device in a row ("a few seconds", the
/// S1b carry-over).
pub const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(3);

/// Why an output's boundary did not go out.
#[derive(Clone, Debug)]
enum Skip {
    /// It waits out a backoff before it builds again.
    Backoff,
    /// No Direct3D / Spout here.
    Unsupported,
    /// A build, the compose or its send failed.
    Failed(GpuError),
}

/// The device loss in `went`, if that is how it failed: both outputs then
/// fail with it (the device's objects are already dropped, once).
fn device_lost<T>(went: &Result<T, Skip>) -> Option<GpuError> {
    match went {
        Err(Skip::Failed(error)) if error.is_device_lost() => Some(error.clone()),
        _ => None,
    }
}

/// A boundary with no compositor after its build step: a worker bug,
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
enum Which {
    /// `SP-program-MAX`, 3840×2160: the target added for it (#279 P1b).
    Max,
    /// `SP-program`, 1920×1080: the compositor's primary target.
    Fhd,
}

/// One output's sender and its backoff: MAX's, of the target added to the
/// compositor with it (#279 P1b: removed with it), or (#239) the
/// `SP-program` one, of the compositor's primary (FHD) target.
struct Output<S> {
    sender: Option<S>,
    /// MAX's target on the compositor (`None` for the FHD output's: the
    /// primary, the compositor's for its life).
    target: Option<TargetId>,
    /// No new sender (MAX: target and sender) before this instant.
    retry_at: Option<Instant>,
}

impl<S> Output<S> {
    fn new() -> Self {
        Self {
            sender: None,
            target: None,
            retry_at: None,
        }
    }
}

/// The one device: its compositor, which draws every output's target.
struct Device<C> {
    compositor: Option<C>,
    /// No new compositor before this instant.
    retry_at: Option<Instant>,
    /// The device was lost and no boundary of either output went out since.
    lost_unsent: bool,
}

/// The log lines one boundary wrote ([`MaxWorker::serve_lines`]).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Lines {
    /// `SP-program-MAX`'s.
    pub max: Option<LogLine>,
    /// #239: the FHD sender's (`None` too while it is not wanted).
    pub fhd: Option<LogLine>,
    /// #279 P1b: the compositor's for NDI alone (`None` while MAX is wanted).
    pub ndi: Option<LogLine>,
    /// #279 P1b: the UYVY pass's own (`program_max_ndi.rs`): its state and
    /// the boundaries held back.
    pub ndi_pass: Option<(PassLine, u64)>,
}

/// The `program-max` thread's state: the GPU objects it built, the picture
/// ids and the backoffs. It reports every boundary to `out`.
pub struct MaxWorker<'a, G: MaxGpu> {
    out: &'a MaxOut,
    gpu: G,
    // Dropped in declaration order: both senders before the compositor
    // (they hold references into its device and its targets).
    /// MAX's sender and its target (#279 P1b), only while wanted.
    max: Output<G::Sender>,
    /// #251: the size and fit MAX's target was added at (`program_max_size`).
    max_built: Target,
    /// #239: the `SP-program` sender, only while wanted.
    fhd: Output<G::Sender>,
    device: Device<G::Compositor>,
    /// #279 P1b: NDI's readback (`program_max_ndi.rs`).
    ndi: ndi::NdiPass,
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
    /// What the sends wait on: [`NoWait`] unless [`with_clock`](Self::with_clock)
    /// gives another (the loop gives [`SpinClock`]).
    clock: Box<dyn SendClock>,
    /// The display refresh, when measured ([`with_vblank`](Self::with_vblank)).
    vblank: Option<Box<dyn VblankSource>>,
    pacer: VblankPacer,
    /// #243 follow-up: Spout's sender list, read after each boundary
    /// ([`with_list`](Self::with_list); none by default).
    list: ListSide,
}

/// A sent MAX boundary's costs and timing ([`MaxWorker::send_max`]).
type Sent = (ComposeStats, SpoutSendStats, SendTiming, Option<Aligned>);

/// #239: a sent FHD boundary: its target's own draw (µs) and its send.
type FhdSent = (u64, SpoutSendStats);

/// One compose of a boundary: its costs, how it was paced and when it
/// started ([`MaxWorker::draw`]).
struct Composed {
    compose: ComposeStats,
    /// #251 step 2: the FHD target's own part of the draw (0 without it).
    fhd_draw_us: u64,
    paced: (Due, Option<VblankGrid>),
    /// When the compose started, and whether its job came after its
    /// instant (#251).
    composing: (Instant, bool),
}

/// What one job did on each output: MAX's boundary (`None` while it is not
/// wanted, #279 P1b), and the FHD sender's (`None` while it is not wanted,
/// or the platform is unsupported).
struct Went {
    max: Option<Result<Sent, Skip>>,
    fhd: Option<Result<FhdSent, Skip>>,
    /// #279 P1b: the compose for NDI alone (MAX not wanted): built and
    /// composed, or why not (`None`: nothing to say, `ndi_report`).
    ndi: Option<Result<(), Skip>>,
    /// The line the UYVY pass's log wrote (`ndi_pass`).
    ndi_pass: Option<(PassLine, u64)>,
}

impl<'a, G: MaxGpu> MaxWorker<'a, G> {
    pub fn new(out: &'a MaxOut, gpu: G) -> Self {
        Self {
            out,
            gpu,
            max: Output::new(),
            max_built: MAX_TARGET,
            fhd: Output::new(),
            device: Device {
                compositor: None,
                retry_at: None,
                lost_unsent: false,
            },
            ndi: ndi::NdiPass::default(),
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
            list: ListSide::new(None, std::process::id()),
        }
    }

    /// #243 follow-up: read who holds Spout's sender list through `list`
    /// after each boundary, and wait on it with its sentinel (production:
    /// `program_max_list::SpoutListLock`).
    pub fn with_list(mut self, list: Box<dyn SpoutList>) -> Self {
        self.list = ListSide::new(Some(list), std::process::id());
        self
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

    /// Whether it holds the compositor (the senders live only with it): an
    /// off setting makes it drop them all.
    pub fn holds_gpu(&self) -> bool {
        self.device.compositor.is_some()
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
    /// `now`, on MAX while it is wanted and (#239) on the FHD sender while
    /// it is wanted, and (#279 P1b) read it back for NDI while that is
    /// wanted; report each and log what changed on each ([`LogGate`]);
    /// returns the lines logged. An output no longer wanted is dropped first
    /// (MAX's with its target).
    pub fn serve_lines(&mut self, job: &MaxJob, offered: Instant, now: Instant) -> Lines {
        let wants = self.out.wants();
        if !wants.fhd && (self.fhd.sender.is_some() || self.fhd.retry_at.is_some()) {
            self.release_fhd();
        }
        if !wants.max && (self.max.sender.is_some() || self.max.retry_at.is_some()) {
            self.release_max();
        }
        self.resize_max();
        if !wants.any() {
            // Taken just before a setting left nothing wanting the
            // compositor: nothing built for it, the next step releases.
            return Lines::default();
        }
        let went = self.attempt(job, offered, now, wants);
        // #251 step 2: the device the FHD side lost after MAX's boundary went
        // out (its send, or its build after MAX's send) was MAX's too.
        let lost = match (&went.max, &went.fhd) {
            (Some(Ok(_)), Some(fhd)) => device_lost(fhd),
            _ => None,
        };
        let max = went
            .max
            .and_then(|went| self.finish_max(went, lost, job, now));
        let fhd = went.fhd.and_then(|went| self.finish_fhd(went, job, now));
        let ndi = went.ndi.and_then(|went| self.finish_ndi(went, job, now));
        self.list.boundary(now, self.out);
        let ndi_pass = went.ndi_pass;
        Lines {
            max,
            fhd,
            ndi,
            ndi_pass,
        }
    }

    /// Report MAX's boundary and log what changed; returns the line it
    /// logged. (Each failure was recovered from where it happened.) A
    /// device `lost` after it went out leaves it counted as sent, and MAX's
    /// state and log name the loss: its next boundaries wait for the device.
    fn finish_max(
        &mut self,
        went: Result<Sent, Skip>,
        lost: Option<GpuError>,
        job: &MaxJob,
        now: Instant,
    ) -> Option<LogLine> {
        match went {
            Ok((compose, send, timing, aligned)) => {
                self.out.record_sent(compose, send);
                self.out.record_send_timing(timing);
                self.out.record_vblank(aligned);
                self.failing = lost.map(|error| error.to_string());
                if let Some(why) = &self.failing {
                    self.out.record_lost(why);
                }
            }
            Err(Skip::Backoff) => self.out.record_skipped(),
            // The build recorded it (`build_device`).
            Err(Skip::Unsupported) => return None,
            Err(Skip::Failed(error)) => {
                let why = error.to_string();
                self.out.record_failed(&why);
                self.failing = Some(why);
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

    /// #239: report the FHD sender's boundary, read where Spout lists it
    /// after its first boundary, and log what changed; returns the line it
    /// logged.
    fn finish_fhd(
        &mut self,
        went: Result<FhdSent, Skip>,
        job: &MaxJob,
        now: Instant,
    ) -> Option<LogLine> {
        match went {
            Ok((draw_us, send)) => {
                self.fhd_failing = None;
                self.out.record_fhd_sent(draw_us, send);
                if self.out.fhd_listed().is_none() {
                    let listed = self.fhd.sender.as_ref().and_then(|s| s.listed_size());
                    self.out.record_fhd_listed(listed);
                }
            }
            Err(Skip::Backoff | Skip::Unsupported) => self.out.record_fhd_skipped(),
            Err(Skip::Failed(error)) => {
                let why = error.to_string();
                self.out.record_fhd_failed(&why);
                self.fhd_failing = Some(why);
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

    /// Build what is missing, each unless its backoff runs (the compositor;
    /// while wanted MAX's target and sender, NDI's pass, and the FHD sender,
    /// after MAX's send unless MAX's boundary is not drawn), then, when an
    /// output can go out or NDI's pass is on: pace `job` FIRST, compose it
    /// into every target at its compose instant (#251, [`draw`](Self::draw):
    /// one upload of each picture; #279 P1b: NDI's readback right after it)
    /// and send it when it is due, MAX's first, then the FHD one's right
    /// after (alone, at the due instant). A lost device fails both outputs
    /// with it.
    fn attempt(&mut self, job: &MaxJob, offered: Instant, now: Instant, wants: Wants) -> Went {
        if self.unsupported {
            return Went {
                max: wants.max.then_some(Err(Skip::Unsupported)),
                fhd: None,
                ndi: None,
                ndi_pass: None,
            };
        }
        let (built, ndi_pass) = self.device_ready(wants.ndi, now);
        if let Err(skip) = built {
            let unsupported = matches!(skip, Skip::Unsupported);
            let fhd = (wants.fhd && !unsupported).then(|| Err(skip.clone()));
            return Went {
                ndi: ndi::ndi_report(wants, Some(Err(skip.clone()))),
                max: wants.max.then_some(Err(skip)),
                fhd,
                ndi_pass,
            };
        }
        let max_built = wants.max.then(|| self.build(Which::Max, now));
        let max_ok = matches!(max_built, Some(Ok(())));
        // #239: MAX goes first, whole. An FHD sender still to build (a
        // Spout open) is built AFTER MAX's send and sends from the next
        // boundary, so its cost never delays this boundary's MAX (an open
        // longer than the wait to the next compose instant makes that
        // compose late: `compose_late`); only when MAX's boundary is not
        // drawn at all (its build failed, lost the device or waits a
        // backoff) is it built first.
        let fhd_first = wants.fhd && (!max_ok || self.fhd.sender.is_some());
        let fhd_built = fhd_first.then(|| match max_built.as_ref().and_then(device_lost) {
            Some(error) => Err(Skip::Failed(error)),
            None => self.build(Which::Fhd, now),
        });
        // A device lost while building the FHD output is the device's: MAX
        // fails with it too, and nothing is drawn.
        let max_built = match fhd_built.as_ref().and_then(device_lost) {
            Some(error) => max_built.map(|_| Err(Skip::Failed(error))),
            None => max_built,
        };
        let max_ok = matches!(max_built, Some(Ok(())));
        let fhd_ready = matches!(fhd_built, Some(Ok(())));
        // The pictures are labelled, paced and drawn only for a boundary
        // that can go out on some output, or that NDI reads back.
        let composed = (max_ok || fhd_ready || self.ndi.on).then(|| self.draw(job, offered, now));
        let max = max_built.map(|built| match (built, &composed) {
            (Err(skip), _) => Err(skip),
            (Ok(()), Some(Ok(drawn))) => self.send_max(drawn, offered, now),
            (Ok(()), Some(Err(skip))) => Err(skip.clone()),
            (Ok(()), None) => Err(Skip::Failed(NO_COMPOSITOR)),
        });
        let lost = max.as_ref().and_then(device_lost);
        let fhd = match fhd_built {
            Some(built) => Some(match (built, &composed, lost) {
                (Err(skip), _, _) => Err(skip),
                (Ok(()), _, Some(error)) => Err(Skip::Failed(error)),
                (Ok(()), Some(Ok(drawn)), None) => self.send_fhd(drawn, offered, now),
                (Ok(()), Some(Err(skip)), None) => Err(skip.clone()),
                (Ok(()), None, None) => Err(Skip::Failed(NO_COMPOSITOR)),
            }),
            None if wants.fhd => self.build_fhd_after(lost, now),
            None => None,
        };
        let drawn = composed.map(|drawn| drawn.map(|_| ()));
        let ndi = ndi::ndi_report(wants, drawn);
        Went {
            max,
            fhd,
            ndi,
            ndi_pass,
        }
    }

    /// #239: MAX went first: build the FHD sender now, sending from the next
    /// boundary. A built output reports nothing for this boundary (it was
    /// not sent); a failure, a backoff or the device MAX's boundary lost is
    /// reported as usual.
    fn build_fhd_after(
        &mut self,
        lost: Option<GpuError>,
        now: Instant,
    ) -> Option<Result<FhdSent, Skip>> {
        if let Some(error) = lost {
            return Some(Err(Skip::Failed(error)));
        }
        self.build(Which::Fhd, now).err().map(Err)
    }

    /// Build the compositor unless it is there or its backoff runs (it
    /// records its adapter). A failed build waits the backoff; a lost device
    /// while building waits it too; `Unsupported` is final (recorded once).
    fn build_device(&mut self, now: Instant) -> Result<(), Skip> {
        if self.device.compositor.is_some() {
            return Ok(());
        }
        if self.device.retry_at.is_some_and(|at| now < at) {
            return Err(Skip::Backoff);
        }
        match self.gpu.compositor() {
            Ok(built) => {
                self.out.record_adapter(built.adapter());
                self.device.compositor = Some(built);
                Ok(())
            }
            Err(GpuError::Unsupported) => {
                self.unsupported = true;
                self.out.record_unsupported();
                Err(Skip::Unsupported)
            }
            Err(error) => {
                if error.is_device_lost() {
                    self.lose_device(now, true);
                } else {
                    self.device.retry_at = Some(now + MAX_RETRY_BACKOFF);
                }
                Err(Skip::Failed(error))
            }
        }
    }

    /// Make `which`'s sender (MAX's target first) unless it is there or its
    /// backoff runs; a failure is recovered from at once
    /// ([`output_failed`](Self::output_failed)).
    fn build(&mut self, which: Which, now: Instant) -> Result<(), Skip> {
        let output = self.output(which);
        if output.sender.is_some() {
            return Ok(());
        }
        if output.retry_at.is_some_and(|at| now < at) {
            return Err(Skip::Backoff);
        }
        let made = match which {
            Which::Max => self.make_max(),
            Which::Fhd => self.make_fhd(),
        };
        made.map_err(|error| {
            self.output_failed(which, &error, now, true);
            Skip::Failed(error)
        })
    }

    /// #279 P1b: MAX's target on the compositor (unless it holds it
    /// already), then the sender `SP-program-MAX` of it.
    fn make_max(&mut self) -> Result<(), GpuError> {
        let wanted = self.max_target();
        let compositor = self.device.compositor.as_mut().ok_or(NO_COMPOSITOR)?;
        let target = match self.max.target {
            Some(target) => target,
            None => {
                let target = compositor.add_target(wanted)?;
                self.max_built = wanted;
                *self.max.target.insert(target)
            }
        };
        self.max.sender = Some(self.gpu.sender(compositor, target)?);
        Ok(())
    }

    /// The sender `SP-program` of the compositor's primary (FHD) target.
    fn make_fhd(&mut self) -> Result<(), GpuError> {
        let compositor = self.device.compositor.as_ref().ok_or(NO_COMPOSITOR)?;
        self.fhd.sender = Some(self.gpu.fhd_sender(compositor, TargetId::PRIMARY)?);
        Ok(())
    }

    fn output(&self, which: Which) -> &Output<G::Sender> {
        match which {
            Which::Max => &self.max,
            Which::Fhd => &self.fhd,
        }
    }

    /// After `which`'s build (`building`) or send failed with `error`: a lost
    /// device drops everything ([`lose_device`](Self::lose_device)); a
    /// refused sender, or a failed build, drops the output's sender (MAX's
    /// target with it) and waits [`MAX_RETRY_BACKOFF`] before its next
    /// build; anything else (one lost frame) costs this boundary only.
    fn output_failed(&mut self, which: Which, error: &GpuError, now: Instant, building: bool) {
        if error.is_device_lost() {
            self.lose_device(now, building);
            return;
        }
        let refused = is_refusal(error);
        if !(refused || building) {
            return;
        }
        let retry_at = Some(now + MAX_RETRY_BACKOFF);
        match which {
            Which::Max => {
                self.drop_max();
                self.max.retry_at = retry_at;
                if refused {
                    self.out.record_sender_backoff();
                }
            }
            Which::Fhd => {
                self.drop_fhd();
                self.fhd.retry_at = retry_at;
                if refused {
                    self.out.record_fhd_sender_backoff();
                }
            }
        }
    }

    /// The device is gone (one event for both outputs): drop both senders
    /// and the compositor, forget the pictures it held, count the reset. A
    /// loss while building, or a second loss before a boundary went out,
    /// waits [`MAX_RETRY_BACKOFF`] before the next build.
    fn lose_device(&mut self, now: Instant, building: bool) {
        self.drop_gpu();
        // No decoded frame stays pinned for a compositor that is gone.
        self.ids.forget();
        self.out.record_device_reset();
        if building || self.device.lost_unsent {
            self.device.retry_at = Some(now + MAX_RETRY_BACKOFF);
        }
        self.device.lost_unsent = true;
    }

    /// Drop MAX's sender, then the FHD one (Spout unregisters the names),
    /// then the compositor with every target (and NDI's pass, #279 P1b).
    fn drop_gpu(&mut self) {
        self.max.sender = None;
        self.fhd.sender = None;
        self.max.target = None;
        self.ndi.forget();
        self.device.compositor = None;
        self.out.record_fhd_listed(None);
    }

    /// #279 P1b: drop MAX's sender (Spout unregisters `SP-program-MAX`),
    /// then take its target off the compositor: nothing is drawn for it.
    fn drop_max(&mut self) {
        self.max.sender = None;
        if let (Some(target), Some(compositor)) =
            (self.max.target.take(), self.device.compositor.as_mut())
        {
            compositor.remove_target(target);
        }
    }

    /// Drop the FHD sender (Spout unregisters `SP-program`); its target,
    /// the primary, stays.
    fn drop_fhd(&mut self) {
        self.fhd.sender = None;
        self.out.record_fhd_listed(None);
    }

    /// Label `job`'s pictures, pace it, wait for its compose instant (#251)
    /// and compose it into every target the compositor holds (the FHD
    /// primary and, while its sender lives, MAX's): each picture uploaded
    /// once; #279 P1b: then, while NDI's pass is on, read it back for NDI
    /// (before any send). A lost device drops everything.
    fn draw(&mut self, job: &MaxJob, offered: Instant, now: Instant) -> Result<Composed, Skip> {
        let composition = self.ids.composition(job);
        let paced = self.pace(offered);
        let composing = self.compose_wait(paced.0.compose_at);
        let Some(compositor) = self.device.compositor.as_mut() else {
            return Err(Skip::Failed(NO_COMPOSITOR));
        };
        match compositor.compose(&composition) {
            Ok(compose) => {
                // #279 P0: what crossed to the GPU, whatever the sends do.
                let (bytes, pictures) = (compose.upload_bytes, compose.uploads);
                self.out.record_uploads(now, bytes, pictures);
                let fhd_draw_us = compositor.draw_us(TargetId::PRIMARY);
                let at = elapsed_100ns(self.started, now);
                let read = self.ndi.on
                    && ndi::read_back(
                        &mut self.ndi,
                        self.out,
                        compositor,
                        job,
                        compose.uyvy_us,
                        at,
                    );
                if read {
                    // NDI's picture came: the device works (a later loss
                    // rebuilds at once).
                    self.device.lost_unsent = false;
                }
                Ok(Composed {
                    compose,
                    fhd_draw_us,
                    paced,
                    composing,
                })
            }
            Err(error) => {
                if error.is_device_lost() {
                    self.lose_device(now, false);
                }
                Err(Skip::Failed(error))
            }
        }
    }

    /// When the boundary offered at `offered` is due: [`VblankPacer`]'s slot
    /// on the refresh grid and (#251) its compose instant, else the
    /// constant lead; and that grid.
    fn pace(&mut self, offered: Instant) -> (Due, Option<VblankGrid>) {
        let now = self.clock.as_mut().now();
        let grid = self.vblank.as_ref().and_then(|v| {
            // #259: the source moves to a new primary when Windows re-lays.
            self.out.record_vblank_output(v.output());
            log_vblank(self.out.record_vblank_state(v.state(now)), &**v);
            v.grid(now)
        });
        // #251 step 4: the SP-program video side waits on the same grid.
        self.out.video.publish_grid(grid, now);
        let send_phase = self.out.vblank_phase();
        let compose_phase = self.out.compose_phase();
        let due = self.pacer.due(offered, grid, send_phase, compose_phase);
        (due, grid)
    }

    /// #251: wait on the clock for the boundary's compose instant (`None`
    /// off the grid: at once); when the compose starts, and whether the job
    /// came after that instant (composed at once: `compose_late`).
    fn compose_wait(&mut self, compose_at: Option<Instant>) -> (Instant, bool) {
        match compose_at {
            Some(at) => wait_for(self.clock.as_mut(), at),
            None => (self.clock.as_mut().now(), false),
        }
    }

    /// Send MAX's boundary, `drawn` at its compose instant, at its due
    /// instant, and how both were paced on the grid; a failure is recovered
    /// from at once.
    fn send_max(&mut self, drawn: &Composed, offered: Instant, now: Instant) -> Result<Sent, Skip> {
        let (due, grid) = drawn.paced;
        let (composed, compose_late) = drawn.composing;
        let Some(sender) = self.max.sender.as_mut() else {
            return Err(Skip::Failed(NO_SENDER));
        };
        sender.set_send_mode(self.out.spout_send());
        let sent = send_at(self.clock.as_mut(), due.at, offered, || sender.send());
        let (send, timing) = sent.map_err(|error| {
            self.output_failed(Which::Max, &error, now, false);
            Skip::Failed(error)
        })?;
        // A boundary went out on this device (a later loss counts afresh).
        self.device.lost_unsent = false;
        let aligned = grid.map(|grid| Aligned {
            period: grid.period,
            phase_us: phase_after_vblank_us(&grid, timing.started),
            compose_phase_us: phase_after_vblank_us(&grid, composed),
            compose_late,
            repicked: due.repicked,
        });
        Ok((drawn.compose, send, timing, aligned))
    }

    /// #239: send the FHD target, `drawn` with MAX's: right after MAX's
    /// send (its due instant has passed), or at the due instant when MAX's
    /// did not go out; never paced on its own. A failure is recovered from
    /// at once.
    fn send_fhd(
        &mut self,
        drawn: &Composed,
        offered: Instant,
        now: Instant,
    ) -> Result<FhdSent, Skip> {
        let Some(sender) = self.fhd.sender.as_mut() else {
            return Err(Skip::Failed(NO_SENDER));
        };
        sender.set_send_mode(self.out.spout_send());
        let sent = send_at(self.clock.as_mut(), drawn.paced.0.at, offered, || {
            sender.send()
        });
        let (send, _) = sent.map_err(|error| {
            self.output_failed(Which::Fhd, &error, now, false);
            Skip::Failed(error)
        })?;
        self.device.lost_unsent = false;
        Ok((drawn.fhd_draw_us, send))
    }
}

/// #243: the one WARN where the paced output stops ticking and the one
/// INFO where it ticks again (the decision is `vblank_log`'s, tested).
/// `mutants::skip`: it only writes the log line.
#[cfg_attr(test, mutants::skip)]
fn log_vblank(log: Option<VblankLog>, source: &dyn VblankSource) {
    match log {
        Some(VblankLog::StoppedTicking) => warn!(
            output = %source.output(),
            "program max: the display output does not tick (its waits do not wait) — sends at the constant lead"
        ),
        Some(VblankLog::TicksAgain) => {
            info!(output = %source.output(), "program max: the display output ticks again");
        }
        None => {}
    }
}

/// The `program-max` thread: take each job from `out` and serve it on
/// `gpu`, each sent at its due instant on the real clock ([`SpinClock`]) —
/// on `vblank`'s refresh grid when one is given —, release the GPU while
/// nothing wants it (MAX off, NDI on the CPU), quiesce and resume (#243
/// follow-up), exit on stop; `list` = Spout's sender list to watch
/// ([`MaxWorker::with_list`]).
pub fn run_max_loop<G: MaxGpu>(
    out: &MaxOut,
    gpu: G,
    vblank: Option<Box<dyn VblankSource>>,
    list: Option<Box<dyn SpoutList>>,
) {
    let _consumer = out.attach();
    let mut worker = MaxWorker::new(out, gpu).with_clock(Box::new(SpinClock));
    if let Some(source) = vblank {
        worker = worker.with_vblank(source);
    }
    if let Some(list) = list {
        worker = worker.with_list(list);
    }
    loop {
        match out.next(worker.holds_gpu()) {
            MaxNext::Job(job, offered) => {
                worker.serve_offered(&job, offered, Instant::now());
            }
            MaxNext::Release => worker.release(Instant::now()),
            MaxNext::Quiesce => worker.quiesce(),
            MaxNext::Resume => worker.resume(),
            MaxNext::Stop => break,
        }
    }
    worker.release(Instant::now());
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

#[cfg(test)]
#[path = "program_max_worker_tests_fhd_lost.rs"]
mod tests_fhd_lost;

#[cfg(test)]
#[path = "program_max_worker_tests_log.rs"]
mod tests_log;

#[cfg(test)]
#[path = "program_max_worker_tests_upload.rs"]
mod tests_upload;

#[cfg(test)]
#[path = "program_max_worker_tests_release.rs"]
mod tests_release;

#[cfg(test)]
#[path = "program_max_worker_tests_ndi.rs"]
mod tests_ndi;
