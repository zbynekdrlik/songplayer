//! #221 L4b: SongPlayer's own program is the PLAYBACK authority (design
//! record 5873773896 §1e). cg OBS's scene detection starts and pauses
//! nothing any more.
//!
//! - **What is on air** is `program_on_air::on_air_set`: `SP-program`'s
//!   source when it is a playlist, together with the playlist SongPlayer last
//!   told cg OBS to show (`legacy_cg.shown`, until B4 step 6).
//! - **The task** ([`run_program_authority`], spawned by `start_program`)
//!   watches the bus's on-air watch and `legacy_cg.shown`. On its first
//!   value (the program restored at startup) and on every change of either,
//!   it sends `PipelineEvent::OnProgram` on the engine's own event channel
//!   (the #215 `SceneOffDue` precedent): OFF for every playlist that left,
//!   then ON for every playlist that entered and for the source of a new
//!   cut (a new `seq`: the re-kick of a press of the scene already on air;
//!   `program_on_air::on_air_changes`). A member nobody cut to is never
//!   re-kicked, so a playlist the operator paused stays paused (review
//!   round 1). It ends on shutdown, or when the engine's channel is gone.
//! - **The engine drops a stale event** ([`PlaybackEngine::on_program`]).
//!   The channel is a queue, so an event can arrive after the set changed
//!   again: ON is applied only while the playlist is on air, OFF only while
//!   it is not. "On air" here is the set the task last DIFFED
//!   ([`OnAirPlaylists`], written before that value's events are sent),
//!   never the live bus: the bus can change and change back between two
//!   task wakes (the watch coalesces), and the task then sends no newer
//!   event for a playlist whose older one the live bus made stale. Against
//!   the diffed set, a dropped event always has a newer one for its
//!   playlist queued behind it (review round 1). So the program's own
//!   source is never taken off program (the old `Hold::OnProgram` wait, for
//!   a cg OBS scene event that came before SongPlayer's cut, is gone), and
//!   an outgoing playlist is held only through its transition window
//!   (`Hold::Until`, `scene_off.rs`).
//! - **One wall owner** (release 0.69.0 review 🟡 2): with each set the task
//!   publishes its `program_on_air::wall_owner` (SP-program's playlist, else
//!   the one cg OBS was told to show). While there is one, only it writes
//!   the shared wall outputs ([`OnAirPlaylists::may_write_wall`]): the
//!   `ShowSubtitles` dispatch and the Presenter push (`position_update.rs`),
//!   the song-end clear (`clear_lyrics.rs`), the title timers
//!   (`title_timers.rs`) and a re-sync's title and lines (`recovery.rs`). The
//!   other member of a two-member set keeps playing but writes none of them.
//! - A pipeline created after its playlist went on air (a runtime
//!   `EnsurePipeline`) goes on program itself (`runtime_pipeline.rs`). An ON
//!   for a playlist with NO pipeline (the #196 startup senders ran out of
//!   their budget) creates it lazily, and it goes on program the same way.
//!
//! A manual ▶ claims nothing (`PlayEvent::Start`, `engine_play.rs`).

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info};

use super::PlaybackEngine;
use super::pipeline::PipelineEvent;
use super::program_bus::ProgramBus;
use super::program_on_air::{on_air_changes, on_air_set, wall_owner};

/// The playlists on air as the authority task last diffed them, and the one
/// of them that owns the wall (`program_on_air::wall_owner`, release 0.69.0
/// review 🟡 2): the task writes a value's set and owner BEFORE it sends
/// that value's events, and the engine's stale check and its wall writers
/// read them (the module doc). Empty, with no owner, until the first value.
/// Shared by the engine (`PlaybackEngine::on_air`) and the task.
#[derive(Clone, Debug, Default)]
pub struct OnAirPlaylists(Arc<Mutex<Diffed>>);

/// What [`OnAirPlaylists`] holds.
#[derive(Debug, Default)]
struct Diffed {
    playlists: BTreeSet<i64>,
    owner: Option<i64>,
}

impl OnAirPlaylists {
    fn diffed(&self) -> MutexGuard<'_, Diffed> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The set and the wall owner of the value the task is about to send the
    /// events of.
    pub fn publish(&self, playlists: BTreeSet<i64>, owner: Option<i64>) {
        *self.diffed() = Diffed { playlists, owner };
    }

    /// Test-only: the diffed set alone, with no owner (every playlist on
    /// program writes the wall, as before the owner).
    #[cfg(test)]
    pub fn replace(&self, playlists: BTreeSet<i64>) {
        self.publish(playlists, None);
    }

    /// Whether `playlist_id` is in the set the task last diffed.
    pub fn contains(&self, playlist_id: i64) -> bool {
        self.diffed().playlists.contains(&playlist_id)
    }

    /// Test-only: the wall owner the task last published.
    #[cfg(test)]
    pub fn owner(&self) -> Option<i64> {
        self.diffed().owner
    }

    /// Whether `playlist_id` may write the shared wall outputs (the lines,
    /// the title, the Presenter): while a playlist owns the wall, only that
    /// one. With none (nothing on air, or before the task's first value) this
    /// restricts nothing: a playlist whose OFF is still queued writes as
    /// before, and its OFF re-syncs the wall (`wall_after_scene_off`).
    pub fn may_write_wall(&self, playlist_id: i64) -> bool {
        self.diffed().owner.is_none_or(|owner| owner == playlist_id)
    }
}

/// The playback authority task (the module doc). Test:
/// `program_authority_tests.rs` (the first value, every change, the re-kick,
/// the union with the cg OBS record, the diffed set, shutdown, a gone
/// engine).
pub async fn run_program_authority(
    bus: Arc<ProgramBus>,
    events: mpsc::UnboundedSender<(i64, PipelineEvent)>,
    diffed: OnAirPlaylists,
    mut shutdown: broadcast::Receiver<()>,
) {
    let mut on_air = bus.on_air();
    let mut shown = bus.legacy_cg().shown();
    let mut previous = BTreeSet::new();
    let mut seen_seq = 0;
    loop {
        let program = on_air.borrow_and_update().clone();
        // A new publication is a cut (a press, a dashboard cut, the startup
        // selection): its source is re-kicked.
        let cut_to = program.source.filter(|_| program.seq != seen_seq);
        seen_seq = program.seq;
        let cg_shown = *shown.borrow_and_update();
        let current = on_air_set(&program, cg_shown);
        let owner = wall_owner(&program, cg_shown);
        diffed.publish(current.clone(), owner);
        for (pid, on) in on_air_changes(&previous, &current, cut_to) {
            if events.send((pid, PipelineEvent::OnProgram(on))).is_err() {
                debug!("program authority: the engine is gone — stopping");
                return;
            }
        }
        log_on_air(&previous, &current, program.source, owner, cg_shown);
        previous = current;
        tokio::select! {
            _ = shutdown.recv() => return,
            changed = on_air.changed() => if changed.is_err() { return },
            changed = shown.changed() => if changed.is_err() { return },
        }
    }
}

/// The log line of one value: INFO when the playlists on air changed, DEBUG
/// for a re-kick of the same set. Logging only.
#[cfg_attr(test, mutants::skip)]
fn log_on_air(
    previous: &BTreeSet<i64>,
    current: &BTreeSet<i64>,
    program: Option<i64>,
    wall_owner: Option<i64>,
    cg_shown: Option<i64>,
) {
    if previous != current {
        info!(
            ?previous,
            ?current,
            ?program,
            ?wall_owner,
            ?cg_shown,
            "program authority: the playlists on air changed"
        );
    } else {
        debug!(
            ?current,
            ?program,
            ?wall_owner,
            ?cg_shown,
            "program authority: on air again (a press of the same scene, or cg OBS confirmed it)"
        );
    }
}

impl PlaybackEngine {
    /// `PipelineEvent::OnProgram(on)` of the authority task: `playlist_id`
    /// went on air (`true`) or left it (`false`). A stale event is dropped
    /// (the module doc): ON only while the playlist is on air, OFF only
    /// while it is not. An ON for a playlist with no pipeline creates the
    /// pipeline, which then goes on program (`ensure_pipeline_for_playlist`).
    pub(super) async fn on_program(&mut self, playlist_id: i64, on: bool) {
        if self.on_air_contains(playlist_id) != on {
            debug!(
                playlist_id,
                on, "program authority: a stale on-program event — ignored"
            );
            return;
        }
        if on && !self.pipelines.contains_key(&playlist_id) {
            info!(
                playlist_id,
                "program authority: on air with no pipeline — creating it"
            );
            self.ensure_pipeline_for_playlist(playlist_id).await;
            return;
        }
        self.handle_scene_change(playlist_id, on).await;
    }

    /// Whether `playlist_id` is on air: in the set the authority task last
    /// diffed (`OnAirPlaylists`); `false` before its first value.
    pub(super) fn on_air_contains(&self, playlist_id: i64) -> bool {
        self.on_air.contains(playlist_id)
    }
}

#[cfg(test)]
#[path = "program_authority_tests.rs"]
mod tests;
