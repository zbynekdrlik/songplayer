//! The `program-max` thread's releases (#223 S2, #239, #279 P1b, #243
//! follow-up) and (#251, 19.10.2026) MAX's size, a child module of
//! `program_max_worker.rs` (moved out for the 1000-line cap): it reaches
//! the worker's private fields.

use std::time::Instant;

use sp_gpu::{MAX_FIT, Target};
use tracing::{info, warn};

use super::{MaxGpu, MaxWorker};

impl<G: MaxGpu> MaxWorker<'_, G> {
    /// Nothing wants the compositor (MAX off and NDI on the CPU), or the
    /// thread stops: drop both senders (Spout unregisters the names), then
    /// the compositor, and forget every backoff; the UYVY pass's log says
    /// off at `now`. NDI's read-back pictures are the `SP-program` sender's
    /// to let go (#279 P1b review round 5: it may still flush the held pair).
    pub fn release(&mut self, now: Instant) {
        if self.holds_gpu() {
            info!("program max: the Spout senders and the compositor are released");
        }
        self.pass_released(now);
        self.drop_gpu();
        self.device.retry_at = None;
        self.max.retry_at = None;
        self.fhd.retry_at = None;
        self.ids.forget();
        self.list.forget(self.out);
    }

    /// #243 follow-up: a deploy is about to kill the process: drop both
    /// senders (Spout unregisters them) and the compositor, stop the list
    /// sentinel, and report `quiesced`. Nothing is offered while the
    /// quiesce holds, so no Spout call runs and the kill can never leave a
    /// Spout lock abandoned.
    pub fn quiesce(&mut self) {
        self.release(Instant::now());
        self.list.stop(self.out);
        self.out.record_quiesced();
        info!("program max: quiesced - no Spout call while the quiesce holds");
    }

    /// #243 follow-up: the quiesce expired and the process is still alive
    /// (the deploy did not kill it): serve again; the next job builds the
    /// senders.
    pub fn resume(&mut self) {
        self.out.record_resumed();
        warn!(
            "program max: the quiesce expired - the process was not stopped; the Spout senders resume"
        );
    }

    /// #239: drop the FHD sender (Spout unregisters `SP-program`) and forget
    /// its backoff: it was switched off, or MAX is off.
    pub(super) fn release_fhd(&mut self) {
        if self.fhd.sender.is_some() {
            info!("program max: the SP-program Spout sender is released");
        }
        self.drop_fhd();
        self.fhd.retry_at = None;
    }

    /// #279 P1b: MAX is off while NDI keeps the compositor: drop MAX's
    /// sender and its target (nothing is drawn for it), forget its backoff.
    pub(super) fn release_max(&mut self) {
        if self.max.sender.is_some() {
            info!(
                "program max: the SP-program-MAX Spout sender is released, its target no longer drawn"
            );
        }
        self.drop_max();
        self.max.retry_at = None;
    }

    /// #251: MAX's target at the size `program_max_size` names, filling its
    /// width ([`MAX_FIT`]).
    pub(super) fn max_target(&self) -> Target {
        let (width, height) = self.out.max_size();
        Target {
            width,
            height,
            fit: MAX_FIT,
        }
    }

    /// #251: `program_max_size` changed while MAX's target is on the
    /// compositor at another size: drop its sender and its target, so this
    /// boundary's build makes both anew at the new size (Spout lists
    /// `SP-program-MAX` at it).
    pub(super) fn resize_max(&mut self) {
        let wanted = self.max_target();
        if self.max.target.is_none() || self.max_built == wanted {
            return;
        }
        info!(
            width = wanted.width,
            height = wanted.height,
            "program max: the size changed - SP-program-MAX is made anew at it"
        );
        self.drop_max();
        self.max.retry_at = None;
    }
}
