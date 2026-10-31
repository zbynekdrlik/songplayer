//! #279 P1b: NDI `SP-program`'s picture from the `program-max` thread (a
//! child module of `program_max_worker.rs`, for the 1000-line cap; it
//! reads the worker's private state). Design: #279 comment 6218637444.
//!
//! - While `program_ndi_source` = `gpu` (`Wants::ndi`) P1a's UYVY pass is
//!   enabled on the compositor's PRIMARY target, the FHD program
//!   (`MaxWorker::ndi_pass`); otherwise it is disabled, so the CPU mode
//!   pays no readback. A failed enable is tried again only after
//!   [`MAX_RETRY_BACKOFF`] (it makes textures and compiles a shader). A
//!   device lost at the enable is lost there ([`MaxWorker::device_ready`],
//!   review round 5 B5-1: with MAX off nothing else composes to find it,
//!   and the removed compositor was kept, its enable failing every backoff
//!   for good): like one lost while building when the compositor was built
//!   for this boundary (the backoff), else like one a compose finds (round
//!   6: rebuilt at the next job if a boundary went out since the last
//!   loss).
//! - The pass has ONE log, a `StateLog` of its state ([`PassLine`]: off,
//!   on, failing), observed at every boundary that holds a compositor and
//!   at each release (review round 5 F2: with MAX off nothing observed it
//!   after NDI's switch to the canvas, and its last line stayed "on"): one
//!   INFO when it goes on or off, one WARN per failing spell, a change
//!   inside the log's window said after it, as the state is then, and no
//!   line outside it (review rounds 3-4). Like `LogGate`, it can stay one
//!   change behind: a release inside the window, or a pass gone with the
//!   device while the rebuild waits its backoff (nothing observes it then).
//! - The thread never lets the feed's pictures go: the `SP-program` sender
//!   does, after the held pair's flush (review round 5 F1: the thread
//!   usually saw the setting first, and the held pair repeated a picture).
//! - After each compose, at the compose instant and BEFORE the Spout sends
//!   (a 67 ms Spout lock wait never delays NDI), [`read_back`] maps the
//!   staging copy (`read_uyvy`, never a wait) into a `frame_pool` buffer
//!   and pushes it into the feed ([`MaxOut::ndi`], `ndi_feed.rs`) under the
//!   boundary's stamp, with the map + copy time and the pass's own part of
//!   the draw. A `MaxJob::Black` reads nothing: the feed gets ONE cached
//!   constant UYVY black. A boundary that is not composed (coalesced, a
//!   failed compose, a lost device, a backoff) pushes nothing: the
//!   `SP-program` sender repeats the last picture.
//! - With MAX off, NDI's compose is the compositor's only one: its failures
//!   (a build, a compose) are logged like MAX's ([`MaxWorker::finish_ndi`],
//!   one WARN when composes stop, one INFO when they come back, through a
//!   `LogGate`), never lost behind MAX's `off`.

use std::sync::Arc;
use std::time::Instant;

use sp_gpu::{FHD_TARGET, GpuError, TargetId};
use tracing::{info, warn};

use super::{
    LogGate, LogLine, MAX_LOG_EVERY_100NS, MAX_RETRY_BACKOFF, MaxCompositor, MaxGpu, MaxWorker,
    Skip, elapsed_100ns,
};
use crate::playback::frame_buf::SharedFrame;
use crate::playback::ndi_feed::{BlackUyvy, uyvy_len};
use crate::playback::program_max::{MaxJob, MaxOut, Wants};
use crate::playback::stat_window::{StateLog, WarnLimiter};

/// What the UYVY pass's log says of it (`pass_line`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PassLine {
    /// NDI comes from the canvas: no pass (also the start).
    #[default]
    Off,
    /// NDI's picture is read back.
    On,
    /// The enable failed, and why; it is tried again after each backoff.
    Failing(Arc<str>),
}

/// The pass's state for its log (pure): off while NDI is not wanted, on
/// while the pass is enabled, else failing with the last enable error (a
/// pass wanted, not on and never refused is off: it is enabled at once).
fn pass_line(wanted: bool, on: bool, failing: Option<&Arc<str>>) -> PassLine {
    match (wanted, on, failing) {
        (true, true, _) => PassLine::On,
        (true, false, Some(why)) => PassLine::Failing(Arc::clone(why)),
        _ => PassLine::Off,
    }
}

/// The worker's NDI readback state.
#[derive(Debug, Default)]
pub(super) struct NdiPass {
    /// The UYVY pass is enabled on the compositor (it goes with it).
    pub(super) on: bool,
    /// No enable before this instant (a failed one waits the backoff);
    /// read only while the pass is not on, and forgotten with it.
    retry_at: Option<Instant>,
    /// The constant UYVY black a Black boundary pushes, built once.
    black: BlackUyvy,
    /// At most one WARN of a failed readback per `MAX_LOG_EVERY_100NS`.
    warn: WarnLimiter,
    /// Why the compose for NDI alone fails (`None`: it composes), and its
    /// log: they outlive a compositor, like MAX's.
    failing: Option<String>,
    log: LogGate,
    /// Why the last enable failed (`None`: none failed yet): read only while
    /// the pass is wanted and not on, which is right after a failed enable
    /// or during its backoff, so it is never an older spell's error; it and
    /// the pass's ONE log outlive a compositor.
    enable_failing: Option<Arc<str>>,
    pass_log: StateLog<PassLine>,
}

impl NdiPass {
    /// The compositor is gone: its pass with it.
    pub(super) fn forget(&mut self) {
        self.on = false;
        self.retry_at = None;
    }
}

impl<G: MaxGpu> MaxWorker<'_, G> {
    /// The compositor built (`build_device`) and its UYVY pass as NDI
    /// `wanted`s it (`ndi_pass`): a device lost at the pass's enable fails
    /// the build (review round 5 B5-1), so the boundary goes on as after a
    /// failed build. Returns the pass log's line too (none when the build
    /// failed: nothing observes the pass then).
    pub(super) fn device_ready(
        &mut self,
        wanted: bool,
        now: Instant,
    ) -> (Result<(), Skip>, Option<(PassLine, u64)>) {
        // The enable of a compositor built for this boundary is part of its
        // build (review round 6).
        let building = self.device.compositor.is_none();
        if let Err(skip) = self.build_device(now) {
            return (Err(skip), None);
        }
        let (lost, line) = self.ndi_pass(wanted, building, now);
        match lost {
            Some(error) => (Err(Skip::Failed(error)), line),
            None => (Ok(()), line),
        }
    }

    /// The UYVY pass on the compositor's primary (FHD) target while NDI is
    /// `wanted`, off otherwise (a backoff forgotten). Nothing without a
    /// compositor. The feed is left alone: the `SP-program` sender lets its
    /// pictures go (review round 5 F1). A device lost at the enable is lost
    /// here (`lose_device`: the compositor goes): `building` (the compositor
    /// was built for this boundary) as a loss while building, which waits
    /// the backoff; else as a loss a compose finds, rebuilt at the next job
    /// unless nothing went out since the last loss (review round 6). Returns
    /// that loss and the line the pass's log wrote (its state and the
    /// boundaries held back): the log is observed at every boundary that
    /// holds a compositor.
    fn ndi_pass(
        &mut self,
        wanted: bool,
        building: bool,
        now: Instant,
    ) -> (Option<GpuError>, Option<(PassLine, u64)>) {
        let Some(compositor) = self.device.compositor.as_mut() else {
            return (None, None);
        };
        let mut lost = None;
        if !wanted {
            if self.ndi.on {
                compositor.disable_uyvy();
            }
            self.ndi.forget();
        } else if !self.ndi.on && self.ndi.retry_at.is_none_or(|retry| now >= retry) {
            match compositor.enable_uyvy(TargetId::PRIMARY) {
                Ok(()) => self.ndi.on = true,
                Err(error) => {
                    self.ndi.retry_at = Some(now + MAX_RETRY_BACKOFF);
                    self.ndi.enable_failing = Some(Arc::from(error.to_string()));
                    lost = error.is_device_lost().then_some(error);
                }
            }
        }
        let state = pass_line(wanted, self.ndi.on, self.ndi.enable_failing.as_ref());
        let line = self.say_pass(state, now);
        if lost.is_some() {
            self.lose_device(now, building);
        }
        (lost, line)
    }

    /// The thread let the GPU go (`release`): the pass went with the
    /// compositor, and its log says "off" (review round 5 F2: with MAX off
    /// nothing observed it after NDI's switch to the canvas, and its last
    /// line stayed "on").
    pub(super) fn pass_released(&mut self, now: Instant) {
        self.say_pass(PassLine::Off, now);
    }

    /// The pass's log at `now`: `state`'s line when one is due (its
    /// `StateLog`), written and returned with the boundaries held back.
    fn say_pass(&mut self, state: PassLine, now: Instant) -> Option<(PassLine, u64)> {
        let at = elapsed_100ns(self.started, now);
        let held_back = self
            .ndi
            .pass_log
            .observe(at, MAX_LOG_EVERY_100NS, state.clone())?;
        log_pass_line(&state, held_back);
        Some((state, held_back))
    }
}

/// What the compose for NDI alone reports (`Went::ndi`): only while NDI
/// is wanted and MAX is not (MAX's own report covers the device otherwise),
/// and only when something was attempted — `drawn` = the build's error or
/// the compose's outcome; `None` (no compose: the pass is not on, its
/// enable already WARNed) says nothing.
pub(super) fn ndi_report(
    wants: Wants,
    drawn: Option<Result<(), Skip>>,
) -> Option<Result<(), Skip>> {
    if wants.ndi && !wants.max { drawn } else { None }
}

impl<G: MaxGpu> MaxWorker<'_, G> {
    /// Log what the compose for NDI alone did (`went`): composed, or failed
    /// (a backoff or `Unsupported` changes nothing); returns the line the
    /// log wrote ([`LogGate`]: each change once per `MAX_LOG_EVERY_100NS`).
    pub(super) fn finish_ndi(
        &mut self,
        went: Result<(), Skip>,
        job: &MaxJob,
        now: Instant,
    ) -> Option<LogLine> {
        match went {
            Ok(()) => self.ndi.failing = None,
            Err(Skip::Failed(error)) => self.ndi.failing = Some(error.to_string()),
            Err(Skip::Backoff | Skip::Unsupported) => {}
        }
        let at = elapsed_100ns(self.started, now);
        let line = self.ndi.log.observe(at, self.ndi.failing.as_deref());
        log_ndi_line(
            line.as_ref(),
            self.ndi.failing.as_deref(),
            job.stamp_100ns(),
        );
        line
    }
}

/// The pass's line: an INFO when it goes on or off, a WARN when it
/// fails (`StateLog`'s decision, tested; logging only).
#[cfg_attr(test, mutants::skip)]
fn log_pass_line(state: &PassLine, held_back: u64) {
    match state {
        PassLine::Off => info!(held_back, "program max: NDI's UYVY readback is off"),
        PassLine::On => info!(
            held_back,
            "program max: NDI's UYVY readback is on (SP-program from the GPU)"
        ),
        PassLine::Failing(why) => warn!(
            error = %why,
            held_back,
            "program max: NDI's UYVY pass could not be enabled - tried again after each backoff"
        ),
    }
}

/// The WARN / INFO of [`MaxWorker::finish_ndi`]'s line (the decision is
/// `LogGate`'s, tested; logging only).
#[cfg_attr(test, mutants::skip)]
fn log_ndi_line(line: Option<&LogLine>, failing: Option<&str>, stamp_100ns: i64) {
    match line {
        Some(LogLine::Failing { held_back }) => warn!(
            error = failing.unwrap_or_default(),
            held_back = *held_back,
            stamp_100ns,
            "program max: the compositor does not compose for NDI (MAX off) - SP-program falls back to the CPU canvas"
        ),
        Some(LogLine::Recovered { held_back }) => info!(
            held_back = *held_back,
            stamp_100ns, "program max: the compositor composes for NDI again"
        ),
        None => {}
    }
}

/// After `job`'s compose: its UYVY picture into `out`'s feed (a Black
/// boundary's the cached black, nothing mapped). `uyvy_us` = the pass's own
/// part of the compose's draw; `at_100ns` = the worker's time (the WARN's
/// rate limit). Whether a picture went into the feed (the device works).
pub(super) fn read_back<C: MaxCompositor>(
    pass: &mut NdiPass,
    out: &MaxOut,
    compositor: &mut C,
    job: &MaxJob,
    uyvy_us: Option<u64>,
    at_100ns: i64,
) -> bool {
    let (width, height) = (FHD_TARGET.width, FHD_TARGET.height);
    let stamp = job.stamp_100ns();
    if let MaxJob::Black { .. } = job {
        out.ndi.push(stamp, pass.black.get(width, height));
        return true;
    }
    let started = Instant::now();
    let mut picture = sp_decoder::frame_pool::take(uyvy_len(width, height));
    match compositor.read_uyvy(&mut picture) {
        Ok(true) => {
            let readback_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
            out.ndi.record_readback(readback_us, uyvy_us);
            out.ndi.push(stamp, SharedFrame::new(picture));
            true
        }
        Ok(false) => false,
        Err(error) => {
            if let Some(suppressed) = pass.warn.admit(at_100ns, MAX_LOG_EVERY_100NS) {
                warn!(
                    %error,
                    suppressed,
                    stamp_100ns = stamp,
                    "program max: NDI's UYVY readback failed - SP-program repeats its last picture"
                );
            }
            false
        }
    }
}
