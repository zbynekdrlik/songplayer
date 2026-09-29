//! #215: the deferred scene-go-off pause (an `impl PlaybackEngine` split out of
//! `mod.rs` for the 1000-line cap).
//!
//! While `SP-program` fades from one playlist to another, the outgoing
//! playlist must keep decoding and emitting until the fade is over: a paused
//! source is a frozen picture and silence, which is exactly the hard on/off the
//! transition removes. So when a playlist leaves program (#221 L4b: the
//! playback authority's OFF, `program_authority.rs`), the engine asks the
//! program bus (`ProgramBus::hold_for`):
//!
//! - `Hold::Until(t)` — it is the `from` of a window (or a cut) that is not
//!   served yet: re-check at `t`, one slot after the window's end. A fade that
//!   still waits for the incoming source's first live pair (the #215 cue
//!   gate) reports the LATEST end it can reach, i.e. the cut plus
//!   `CUE_WAIT_MAX_SLOTS` plus its slots, so the outgoing playlist keeps
//!   playing through the wait too; the re-check then finds the window over,
//!   or asks again;
//! - no hold — pause now, exactly as before.
//!
//! The authority never takes the program's own source off program (its OFF
//! would be stale), so #221 L4b deleted the `Hold::OnProgram` wait (one
//! `CUT_SETTLE` for a cut that followed cg OBS's scene event).
//!
//! The re-check comes back on the engine's own event channel as
//! `PipelineEvent::SceneOffDue`. If the scene came back on program in the
//! meantime, it does nothing.
//!
//! A HELD playlist (its re-check pending, `PlaylistPipeline::scene_off_due`)
//! is off program, so it has no side effects (release 0.68.0 blockers,
//! design record 5863318980): no lyrics line goes out
//! (`dispatch_lyrics_if_changed`), and its song's end, a failure or a skip
//! PAUSE it at once (`pause_if_held`) instead of starting a song off program:
//! that song's `record_play`, its title timers (whose hide took down the
//! title of the playlist on program) and the ungated song-end clear of the
//! shared subtitle clips never happen. The program bus then mixes the rest
//! of the window out of the paused side's standby (its frozen frame or idle
//! black, and silence).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tracing::{debug, info};

use super::PlaybackEngine;
use super::pipeline::PipelineEvent;
use super::program_bus::Hold;
use super::state::{PlayEvent, PlayState};
use super::wallclock::utc_now_100ns;
use crate::resolume::ResolumeCommand;

/// The id of each hold's re-check (`SceneOffDue`), unique in the process.
/// Not a tokio task id: tokio may reuse one once its task has ended, which
/// is exactly a stale re-check's state (review round 3).
static NEXT_RE_CHECK: AtomicU64 = AtomicU64::new(1);

/// How long to keep a playlist playing after it left program; `None` =
/// pause it now.
pub fn scene_off_delay(hold: Option<Hold>, now_100ns: i64) -> Option<Duration> {
    let Hold::Until(until_100ns) = hold?;
    let wait = until_100ns - now_100ns;
    (wait > 0).then(|| Duration::from_nanos(wait as u64 * 100))
}

impl PlaybackEngine {
    /// The wall after a playlist went off program (its `scene_active` is
    /// already false). Its line goes (`HideSubtitles`). Its title goes too,
    /// unless another playlist is still on program: then the wall is
    /// re-synced to that one — its due title (`resync_wall_title`), and its
    /// line re-sent on its next position report (its subtitle dedup key is
    /// cleared). #221 L4b review round 3: the playback authority sends the
    /// incoming ON at the press and the outgoing OFF only when cg OBS
    /// confirms the mirror, so the incoming playlist's title can be up
    /// already, and a bare `HideTitle` took it down for the rest of its song
    /// (before L4b the bridge sent the OFF first).
    pub(super) async fn wall_after_scene_off(&mut self) {
        let mut others_on_program = false;
        for pp in self.pipelines.values_mut() {
            if pp.scene_active.load(Ordering::Acquire) {
                pp.last_resolume_subtitles_signature = None;
                others_on_program = true;
            }
        }
        let _ = self.resolume_tx.try_send(ResolumeCommand::HideSubtitles);
        if others_on_program {
            let title = self.resync_wall_title().await;
            debug!(
                ?title,
                "scene off program — the wall re-synced to the playlist still on program"
            );
        } else {
            let _ = self.resolume_tx.try_send(ResolumeCommand::HideTitle);
        }
    }

    /// The scene-go-off half of `handle_scene_change`: pause, unless the
    /// program bus holds the playlist through a transition.
    pub(super) async fn scene_off(&mut self, playlist_id: i64) {
        self.scene_off_step(playlist_id, utc_now_100ns()).await;
    }

    /// `PipelineEvent::SceneOffDue` of the hold re-check `due`:
    /// re-check a held pause; nothing when the scene came back on program (or
    /// the pipeline is gone), or when `due` is not the pending re-check. A
    /// hold registers its re-check before the engine can see the event, so
    /// any other one is stale: a newer hold replaced it (queued during an
    /// A→B→A→B, it re-checked the newer hold early, review round 1), or the
    /// hold ended (a pause, a scene-on, an operator's pick, which it held again
    /// or paused, review round 2).
    pub(super) async fn scene_off_due(&mut self, playlist_id: i64, due: u64) {
        let stale = self
            .pipelines
            .get(&playlist_id)
            .and_then(|pp| pp.scene_off_due.as_ref())
            .is_none_or(|(pending, _)| *pending != due);
        if stale {
            debug!(playlist_id, "a stale hold re-check — ignored");
            return;
        }
        self.scene_off_recheck(playlist_id, utc_now_100ns()).await;
    }

    /// [`Self::scene_off_due`] at `now_100ns` (the stamps' wall clock). A
    /// playlist back ON AIR in the playback authority's diffed set is left
    /// alone even while its scene is still off: its ON is queued behind this
    /// re-check, and its scene-on ends the hold (#221 L4b review round 2).
    async fn scene_off_recheck(&mut self, playlist_id: i64, now_100ns: i64) {
        let off = self
            .pipelines
            .get(&playlist_id)
            .is_some_and(|pp| !pp.scene_active.load(Ordering::Acquire))
            && !self.on_air_contains(playlist_id);
        if off {
            self.scene_off_step(playlist_id, now_100ns).await;
        }
    }

    /// Pause `playlist_id` now, or re-check once its hold is over (see the
    /// module doc).
    async fn scene_off_step(&mut self, playlist_id: i64, now_100ns: i64) {
        let playing = self
            .pipelines
            .get(&playlist_id)
            .is_some_and(|pp| matches!(pp.state, PlayState::Playing { .. }));
        let hold = self
            .program
            .get()
            .filter(|_| playing)
            .and_then(|bus| bus.hold_for(playlist_id));
        let Some(delay) = scene_off_delay(hold, now_100ns) else {
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
        let re_check = NEXT_RE_CHECK.fetch_add(1, Ordering::Relaxed);
        let task = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tx.send((playlist_id, PipelineEvent::SceneOffDue(re_check)));
        })
        .abort_handle();
        if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
            pp.end_hold(); // a re-check of an earlier hold is superseded
            pp.scene_off_due = Some((re_check, task));
        }
    }

    /// A playlist HELD off program (see the module doc) whose song ended,
    /// failed or was skipped: pause it now, as the hold's end would (a
    /// `SceneOff`: the Pause cancels the hold's re-check and the title
    /// timers), instead of starting a song off program. Returns whether it
    /// was held; `false` = the caller goes on as usual.
    pub(super) async fn pause_if_held(&mut self, playlist_id: i64, why: &'static str) -> bool {
        let held = self
            .pipelines
            .get(&playlist_id)
            .is_some_and(|pp| pp.scene_off_due.is_some());
        if held {
            info!(
                playlist_id,
                why, "held off program through a transition — paused now, no song starts there"
            );
            self.apply_event(playlist_id, PlayEvent::SceneOff).await;
        }
        held
    }
}

impl super::PlaylistPipeline {
    /// The hold is over (its pause, a scene back on program, an operator's
    /// pick, or a newer hold): its pending re-check is cancelled.
    pub(super) fn end_hold(&mut self) {
        if let Some((_, task)) = self.scene_off_due.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
#[path = "scene_off_tests.rs"]
mod tests;
