//! #215: the deferred scene-go-off pause (an `impl PlaybackEngine` split out of
//! `mod.rs` for the 1000-line cap).
//!
//! While `SP-program` fades from one playlist to another, the outgoing
//! playlist must keep decoding and emitting until the fade is over: a paused
//! source is a frozen picture and silence, which is exactly the hard on/off the
//! transition removes. So when a playlist's OBS scene leaves program, the engine
//! asks the program bus (`ProgramBus::hold_for`):
//!
//! - `Hold::Until(t)` — it is the `from` of a window (or a cut) that is not
//!   served yet: re-check at `t`, one slot after the window's end;
//! - `Hold::OnProgram` — it is still the program's source, and the cut away
//!   from it may be on its way: the follow task and the #213 remote control cut
//!   only AFTER cg OBS switched, and cg OBS's scene event reaches the engine
//!   first. Re-check once after [`CUT_SETTLE`];
//! - no hold — pause now, exactly as before. Every playlist that is not the
//!   program's source takes this path.
//!
//! The re-check comes back on the engine's own event channel as
//! `PipelineEvent::SceneOffDue`. If the scene came back on program in the
//! meantime, it does nothing.

use std::sync::atomic::Ordering;
use std::time::Duration;

use tracing::info;

use super::PlaybackEngine;
use super::pipeline::PipelineEvent;
use super::program_bus::Hold;
use super::state::{PlayEvent, PlayState};
use super::wallclock::utc_now_100ns;

/// How long a playlist that is still on program keeps playing after its OBS
/// scene left, so that the cut following cg OBS lands first (the follow task's
/// or the remote control's persist + cut, tens of ms).
pub const CUT_SETTLE: Duration = Duration::from_millis(500);

/// How long to keep a playlist playing after its OBS scene left program;
/// `None` = pause it now. `settled` = the one [`CUT_SETTLE`] wait was given.
pub fn scene_off_delay(hold: Option<Hold>, now_100ns: i64, settled: bool) -> Option<Duration> {
    match hold? {
        Hold::Until(until_100ns) => {
            let wait = until_100ns - now_100ns;
            (wait > 0).then(|| Duration::from_nanos(wait as u64 * 100))
        }
        Hold::OnProgram => (!settled).then_some(CUT_SETTLE),
    }
}

impl PlaybackEngine {
    /// The scene-go-off half of `handle_scene_change`: pause, unless the
    /// program bus holds the playlist through a transition.
    pub(super) async fn scene_off(&mut self, playlist_id: i64) {
        self.scene_off_step(playlist_id, false).await;
    }

    /// `PipelineEvent::SceneOffDue`: re-check a held pause; nothing when the
    /// scene came back on program (or the pipeline is gone).
    pub(super) async fn scene_off_due(&mut self, playlist_id: i64) {
        let off = self
            .pipelines
            .get(&playlist_id)
            .is_some_and(|pp| !pp.scene_active.load(Ordering::Acquire));
        if off {
            self.scene_off_step(playlist_id, true).await;
        }
    }

    async fn scene_off_step(&mut self, playlist_id: i64, settled: bool) {
        let playing = self
            .pipelines
            .get(&playlist_id)
            .is_some_and(|pp| matches!(pp.state, PlayState::Playing { .. }));
        let hold = self
            .program
            .get()
            .filter(|_| playing)
            .and_then(|bus| bus.hold_for(playlist_id));
        let Some(delay) = scene_off_delay(hold, utc_now_100ns(), settled) else {
            self.apply_event(playlist_id, PlayEvent::SceneOff).await;
            return;
        };
        info!(
            playlist_id,
            ?hold,
            delay_ms = delay.as_millis() as u64,
            "scene off program — the playlist keeps playing through the program transition"
        );
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tx.send((playlist_id, PipelineEvent::SceneOffDue));
        });
    }
}

#[cfg(test)]
#[path = "scene_off_tests.rs"]
mod tests;
