//! The pipeline-lifetime paced submit thread (#147, design record 5845527884,
//! Approach 1 (a)), split out of `submitter.rs` for the 1000-line cap.
//!
//! The paced submit consumer used to live per song / per idle stretch, borrowed
//! through a `thread::scope`, so nobody serviced the grid between two scopes.
//! It now lives as long as the pipeline's `FrameSubmitter`: the first paced
//! scope spawns it here, every later scope only attaches a feeder to its
//! handoff. `pipeline.rs` keeps owning the submitter by value (it is at the
//! 1000-line cap), so the thread cannot borrow it; instead it owns a twin
//! `FrameSubmitter` built on a NON-OWNING twin of this submitter's NDI sender
//! (`NdiSender::twin`). The `paced_output` field is declared first, so the
//! thread is stopped, drained, flushed and joined before this submitter's
//! owning sender destroys the NDI instance.

use std::sync::Arc;

use sp_ndi::NdiBackend;

use super::FrameSubmitter;
use crate::playback::paced_output::{PacedConsumer, PacedOutput, Picture, SharedHandoff};
use crate::playback::wallclock::WallClock;

impl<B: NdiBackend + 'static> FrameSubmitter<B> {
    /// The handoff of this pipeline's paced submit thread, spawning the thread
    /// on the first call. Its fill black is this submitter's cached
    /// `black_w`×`black_h` standby black (the idle fill's picture); every later
    /// call returns the same handoff. A thread that is gone (it only exits on
    /// stop, so: it panicked) is joined, logged and respawned here, so a dead
    /// submit side costs at most the rest of one scope, never the output.
    pub fn paced_handoff(
        &mut self,
        playlist_id: i64,
        black_w: u32,
        black_h: u32,
    ) -> Arc<SharedHandoff> {
        if let Some(output) = &self.paced_output {
            if !output.is_finished() {
                return output.handoff();
            }
            tracing::error!(
                playlist_id,
                "paced submit thread is gone — respawning it for this scope (#147)"
            );
        }
        // Dropping a finished output joins it (and logs its panic, if any).
        self.paced_output = None;
        let black = Picture {
            width: black_w,
            height: black_h,
            stride: black_w,
            video: self.standby_black_nv12(black_w, black_h),
        };
        let mut twin =
            FrameSubmitter::new(self.sender.twin(), self.frame_rate_n, self.frame_rate_d);
        twin.set_paced(self.paced);
        twin.set_burn_flag(self.burn_on.clone());
        let consumer = PacedConsumer::new(twin, playlist_id, WallClock::system(), black);
        let output = PacedOutput::spawn(consumer);
        let handoff = output.handoff();
        self.paced_output = Some(output);
        handoff
    }
}
