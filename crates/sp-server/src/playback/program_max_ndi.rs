//! #279 P1b: NDI `SP-program`'s picture from the `program-max` thread (a
//! child module of `program_max_worker.rs`, for the 1000-line cap; it
//! reads the worker's private state). Design: #279 comment 6218637444.
//!
//! - While `program_ndi_source` = `gpu` ([`MaxOut::ndi_wanted`]) P1a's
//!   UYVY pass is enabled on the compositor's PRIMARY target, the FHD
//!   program ([`MaxWorker::ndi_pass`]); otherwise it is disabled, so the
//!   CPU mode pays no readback. A failed enable is tried again only after
//!   [`MAX_RETRY_BACKOFF`] (it makes textures and compiles a shader); a lost
//!   device is left to the compose's own check.
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

use std::time::Instant;

use sp_gpu::{FHD_TARGET, TargetId};
use tracing::{info, warn};

use super::{
    LogGate, LogLine, MAX_LOG_EVERY_100NS, MAX_RETRY_BACKOFF, MaxCompositor, MaxGpu, MaxWorker,
    Skip, elapsed_100ns,
};
use crate::playback::frame_buf::SharedFrame;
use crate::playback::ndi_feed::{BlackUyvy, uyvy_len};
use crate::playback::program_max::{MaxJob, MaxOut, Wants};
use crate::playback::stat_window::WarnLimiter;

/// The worker's NDI readback state.
#[derive(Debug, Default)]
pub(super) struct NdiPass {
    /// The UYVY pass is enabled on the compositor (it goes with it).
    pub(super) on: bool,
    /// No enable before this instant (a failed one waits the backoff).
    retry_at: Option<Instant>,
    /// The constant UYVY black a Black boundary pushes, built once.
    black: BlackUyvy,
    /// At most one WARN of a failed readback per `MAX_LOG_EVERY_100NS`.
    warn: WarnLimiter,
    /// Why the compose for NDI alone fails (`None`: it composes), and its
    /// log: they outlive a compositor, like MAX's.
    failing: Option<String>,
    log: LogGate,
    /// The enable's own log: a pass that keeps failing to enable is said
    /// once (not at every retry), its recovery once.
    enable_log: LogGate,
}

impl NdiPass {
    /// The compositor is gone: its pass with it.
    pub(super) fn forget(&mut self) {
        self.on = false;
        self.retry_at = None;
    }
}

impl<G: MaxGpu> MaxWorker<'_, G> {
    /// The UYVY pass on the compositor's primary (FHD) target while NDI is
    /// `wanted`, off otherwise (a backoff forgotten). Nothing without a
    /// compositor. Returns the line the enable's log wrote.
    pub(super) fn ndi_pass(&mut self, wanted: bool, now: Instant) -> Option<LogLine> {
        let compositor = self.device.compositor.as_mut()?;
        if !wanted {
            if self.ndi.on {
                compositor.disable_uyvy();
                info!("program max: NDI's UYVY readback is off");
            }
            self.ndi.forget();
            // Nothing takes them any more: no 4 MB buffer stays pinned.
            self.out.ndi.clear();
            return None;
        }
        if self.ndi.on || self.ndi.retry_at.is_some_and(|at| now < at) {
            return None;
        }
        let at = elapsed_100ns(self.started, now);
        match compositor.enable_uyvy(TargetId::PRIMARY) {
            Ok(()) => {
                self.ndi.on = true;
                self.ndi.retry_at = None;
                info!("program max: NDI's UYVY readback is on (SP-program from the GPU)");
                self.ndi.enable_log.observe(at, None)
            }
            Err(error) => {
                self.ndi.retry_at = Some(now + MAX_RETRY_BACKOFF);
                let why = error.to_string();
                let line = self.ndi.enable_log.observe(at, Some(&why));
                log_enable_failed(line.as_ref(), &why);
                line
            }
        }
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

/// The WARN of a UYVY pass that could not be enabled, once per failing
/// spell (`LogGate`'s decision, tested; logging only).
#[cfg_attr(test, mutants::skip)]
fn log_enable_failed(line: Option<&LogLine>, why: &str) {
    if let Some(LogLine::Failing { held_back }) = line {
        warn!(
            error = why,
            held_back = *held_back,
            "program max: NDI's UYVY pass could not be enabled - tried again after each backoff"
        );
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
