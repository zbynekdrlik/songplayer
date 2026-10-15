//! What one composed frame and one Spout send cost: the telemetry S2
//! publishes as `max.upload_us_p99` / `max.draw_us_p99` /
//! `max.send_us_p99` (#223 R3-2), and (#251 step 2) each target's own part
//! of the draw (`max.fhd.draw_us_p99`).

use std::time::Instant;

/// The cost of one `Compositor::compose` call, measured on the calling
/// thread.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ComposeStats {
    /// The CPU time of the plane uploads (texture creation included), µs:
    /// each picture once, whatever the targets (#251 step 2).
    pub upload_us: u64,
    /// From the first draw command until the GPU has finished the frame,
    /// every target (the GPU side of the uploads included), µs.
    pub draw_us: u64,
    /// How many pictures this call uploaded: 0 (all resident), 1 or 2.
    pub uploads: u32,
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
