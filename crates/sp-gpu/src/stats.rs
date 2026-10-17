//! What one composed frame and one Spout send cost: the telemetry S2
//! publishes as `max.upload_us_p99` / `max.draw_us_p99` /
//! `max.send_us_p99` (#223 R3-2), and (#251 step 2) each target's own part
//! of the draw (`max.fhd.draw_us_p99`); #279 P1a: the UYVY pass's part
//! (`ComposeStats::uyvy_us`).

use std::time::Instant;

use crate::ring_state::Acquired;

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
    /// #279 P2: the ring slots whose producer device was lost
    /// (`WAIT_ABANDONED`), whose acquire failed or that did not open: drawn
    /// black, and their ring is dead.
    pub abandoned: u32,
}

impl ComposeStats {
    /// #279 P2: count one ring slot of a compose whose keyed mutex answered
    /// `acquired`, and say whether it is drawn: held, yes
    /// ([`gpu_sides`](Self::gpu_sides)); busy, no
    /// ([`km_acquire_timeouts`](Self::km_acquire_timeouts)); abandoned or
    /// failed, no ([`abandoned`](Self::abandoned): its ring dies,
    /// `Acquired::kills_ring`).
    pub fn count_gpu_side(&mut self, acquired: Acquired) -> bool {
        match acquired {
            Acquired::Held => {
                self.gpu_sides += 1;
                true
            }
            Acquired::Busy => {
                self.km_acquire_timeouts += 1;
                false
            }
            Acquired::Abandoned | Acquired::Failed(_) => {
                self.abandoned += 1;
                false
            }
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
