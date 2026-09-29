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
//!   then ON for every member (`program_on_air::on_air_changes`, the
//!   re-kick). It ends on shutdown, or when the engine's channel is gone.
//! - **The engine drops a stale event** ([`PlaybackEngine::on_program`]).
//!   The channel is a queue, so an event can arrive after the set changed
//!   again: ON is applied only while the playlist is on air, OFF only while
//!   it is not. So the program's own source is never taken off program (the
//!   old `Hold::OnProgram` wait, for a cg OBS scene event that came before
//!   SongPlayer's cut, is gone), and an outgoing playlist is held only
//!   through its transition window (`Hold::Until`, `scene_off.rs`).
//! - A pipeline created after its playlist went on air (a runtime
//!   `EnsurePipeline`) goes on program itself (`runtime_pipeline.rs`).
//!
//! A manual ▶ claims nothing (`PlayEvent::Start`, `engine_play.rs`).

use std::collections::BTreeSet;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info};

use super::PlaybackEngine;
use super::pipeline::PipelineEvent;
use super::program_bus::ProgramBus;
use super::program_on_air::{on_air_changes, on_air_set};

/// The playback authority task (the module doc). Test:
/// `program_authority_tests.rs` (the first value, every change, the re-kick,
/// the union with the cg OBS record, shutdown, a gone engine).
pub async fn run_program_authority(
    bus: Arc<ProgramBus>,
    events: mpsc::UnboundedSender<(i64, PipelineEvent)>,
    mut shutdown: broadcast::Receiver<()>,
) {
    let mut on_air = bus.on_air();
    let mut shown = bus.legacy_cg().shown();
    let mut previous = BTreeSet::new();
    loop {
        let program = on_air.borrow_and_update().clone();
        let cg_shown = *shown.borrow_and_update();
        let current = on_air_set(&program, cg_shown);
        for (pid, on) in on_air_changes(&previous, &current) {
            if events.send((pid, PipelineEvent::OnProgram(on))).is_err() {
                debug!("program authority: the engine is gone — stopping");
                return;
            }
        }
        log_on_air(&previous, &current, program.source, cg_shown);
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
    cg_shown: Option<i64>,
) {
    if previous != current {
        info!(
            ?previous,
            ?current,
            ?program,
            ?cg_shown,
            "program authority: the playlists on air changed"
        );
    } else {
        debug!(
            ?current,
            ?program,
            ?cg_shown,
            "program authority: on air again (a press of the same scene, or cg OBS confirmed it)"
        );
    }
}

impl PlaybackEngine {
    /// `PipelineEvent::OnProgram(on)` of the authority task: `playlist_id`
    /// went on air (`true`) or left it (`false`). A stale event is dropped
    /// (the module doc): ON only while the playlist is on air now, OFF only
    /// while it is not.
    pub(super) async fn on_program(&mut self, playlist_id: i64, on: bool) {
        if self.on_air_contains(playlist_id) != on {
            debug!(
                playlist_id,
                on, "program authority: a stale on-program event — ignored"
            );
            return;
        }
        self.handle_scene_change(playlist_id, on).await;
    }

    /// Whether `playlist_id` is on air now (`on_air_set` of the program
    /// bus); `false` while no bus is set (before `start_program`).
    pub(super) fn on_air_contains(&self, playlist_id: i64) -> bool {
        self.program.get().is_some_and(|bus| {
            let cg_shown = bus.legacy_cg().shown_now();
            on_air_set(&bus.on_air_now(), cg_shown).contains(&playlist_id)
        })
    }
}

#[cfg(test)]
#[path = "program_authority_tests.rs"]
mod tests;
