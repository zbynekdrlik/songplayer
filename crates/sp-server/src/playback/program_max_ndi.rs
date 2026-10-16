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

use std::time::Instant;

use sp_gpu::{FHD_TARGET, TargetId};
use tracing::{info, warn};

use super::{MAX_LOG_EVERY_100NS, MAX_RETRY_BACKOFF, MaxCompositor, MaxGpu, MaxWorker};
use crate::playback::frame_buf::SharedFrame;
use crate::playback::ndi_feed::{BlackUyvy, uyvy_len};
use crate::playback::program_max::{MaxJob, MaxOut};
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
    /// compositor.
    pub(super) fn ndi_pass(&mut self, wanted: bool, now: Instant) {
        let Some(compositor) = self.device.compositor.as_mut() else {
            return;
        };
        if !wanted {
            if self.ndi.on {
                compositor.disable_uyvy();
                info!("program max: NDI's UYVY readback is off");
            }
            self.ndi.forget();
            return;
        }
        if self.ndi.on || self.ndi.retry_at.is_some_and(|at| now < at) {
            return;
        }
        match compositor.enable_uyvy(TargetId::PRIMARY) {
            Ok(()) => {
                self.ndi.on = true;
                self.ndi.retry_at = None;
                info!("program max: NDI's UYVY readback is on (SP-program from the GPU)");
            }
            Err(error) => {
                self.ndi.retry_at = Some(now + MAX_RETRY_BACKOFF);
                warn!(%error, "program max: NDI's UYVY pass could not be enabled - tried again after the backoff");
            }
        }
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
