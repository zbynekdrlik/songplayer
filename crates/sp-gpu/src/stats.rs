//! What one composed frame and one Spout send cost: the telemetry S2
//! publishes as `max.upload_us_p99` / `max.draw_us_p99` /
//! `max.send_us_p99` (#223 R3-2), and (#251 step 2) each target's own part
//! of the draw (`max.fhd.draw_us_p99`); #279 P1a: the UYVY pass's part
//! (`ComposeStats::uyvy_us`).

use std::time::Instant;

use crate::ring_state::{SlotDraw, SlotOpen};

/// The cost of one `Compositor::compose` call, measured on the calling
/// thread.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ComposeStats {
    /// The CPU time of the plane uploads (texture creation included), µs:
    /// each picture once, whatever the targets (#251 step 2).
    pub upload_us: u64,
    /// From the first draw command until the GPU has finished the frame,
    /// every target (the GPU side of the uploads included), µs; #279 P1a:
    /// with a UYVY pass enabled, the pass and its staging copy too.
    pub draw_us: u64,
    /// How many pictures this call uploaded: 0 (all resident), 1 or 2.
    pub uploads: u32,
    /// #279 P0: the plane bytes those uploads wrote (`Planes::upload_bytes`
    /// of each, 0 when every picture was resident): what crossed from RAM
    /// to the GPU, the number the GPU-resident path must take to 0.
    pub upload_bytes: u64,
    /// #279 P1a: the UYVY pass's own part of the draw, µs (its pass and its
    /// staging copy, from the end of the last target until the GPU had
    /// finished them, as [`target_draws_us`] counts a target's); `None`
    /// when no UYVY pass is enabled (`Compositor::enable_uyvy`).
    pub uyvy_us: Option<u64>,
    /// #279 P2: the ring slots this compose drew from in place
    /// (`Composition::frames`: each slot once), their keyed mutex held.
    pub gpu_sides: u32,
    /// #279 P2: the ring slots this compose opened on its device (a slot is
    /// opened once, then kept: `ring_state::OpenedKeys`).
    pub gpu_opens: u32,
    /// #279 P2: the ring slots whose keyed mutex another side still held
    /// (`WAIT_TIMEOUT` on the 0 ms acquire): drawn black this boundary.
    pub km_acquire_timeouts: u32,
    /// #279 P2: the ring slots whose key a device was lost holding
    /// (`WAIT_ABANDONED`: the producer's, or the compositor's own before its
    /// rebuild) or whose acquire failed on a healthy compositor, and the
    /// slots of a dead ring: drawn black, their ring dead.
    pub abandoned: u32,
    /// #279 P2: the ring slots that did not open on a healthy compositor:
    /// drawn black, their ring killed.
    pub open_failures: u32,
}

impl ComposeStats {
    /// #279 P2: count one ring slot of a compose: what the compositor
    /// found of it (`open`) and what it does with it (`draw`,
    /// `ring_state::slot_draw`). Drawn ([`gpu_sides`](Self::gpu_sides));
    /// busy ([`km_acquire_timeouts`](Self::km_acquire_timeouts)); a dead
    /// ring's, abandoned or a failed acquire
    /// ([`abandoned`](Self::abandoned)); not opened
    /// ([`open_failures`](Self::open_failures)); the compositor's own lost
    /// device counts nothing (the compose fails).
    pub fn count_slot(&mut self, open: SlotOpen, draw: SlotDraw) {
        match (draw, open) {
            (SlotDraw::Draw, _) => self.gpu_sides += 1,
            (SlotDraw::Black, _) => self.km_acquire_timeouts += 1,
            (SlotDraw::Gone, _) | (SlotDraw::KillRing, SlotOpen::Acquired(_)) => {
                self.abandoned += 1;
            }
            (SlotDraw::KillRing, _) => self.open_failures += 1,
            (SlotDraw::DeviceLost, _) => {}
        }
    }
}

/// #251 step 2: each target's own part of one compose's draw, µs, in draw
/// order: the first from `start` (the first draw command) until the GPU had
/// finished it, each next one from the end of the one before until it was
/// finished. `done` = the instants the wait saw each target finished, in
/// draw order (one context runs its commands in order, so they never go
/// back; a pair that did would read 0).
pub fn target_draws_us(start: Instant, done: &[Instant]) -> Vec<u64> {
    let mut from = start;
    done.iter()
        .map(|&end| {
            let part = end.saturating_duration_since(from);
            from = end;
            u64::try_from(part.as_micros()).unwrap_or(u64::MAX)
        })
        .collect()
}

/// #279 P1a: one compose's parts ([`target_draws_us`] over the targets'
/// event queries, then the UYVY pass's when one ran) split into the
/// targets' own, in draw order, and the pass's: the part right after the
/// `targets` targets' parts, `None` when there is none.
pub fn split_uyvy_part(mut parts: Vec<u64>, targets: usize) -> (Vec<u64>, Option<u64>) {
    let uyvy = parts.get(targets).copied();
    parts.truncate(targets);
    (parts, uyvy)
}

/// The cost of one `SpoutSender::send` (#223 S1b), measured on the calling
/// thread.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpoutSendStats {
    /// Spout's `SendTexture` until the GPU has done its copy, µs: the wait
    /// for the sender's named mutex (Spout gives up after 67 ms), the copy
    /// into Spout's shared texture, the flush and the wait for the GPU to
    /// finish it (#223 follow-up); at the first send also the shared
    /// texture's creation and the registration.
    pub send_us: u64,
    /// #243: Spout's list had lost the sender's name and this send put it
    /// back (`spout_state::list_check`).
    pub relisted: bool,
}

#[cfg(test)]
#[path = "stats_tests.rs"]
mod tests;
