//! What `SP-program` has on air (#221 L1, design record 5873773896 §1d): the
//! one "what is on program" signal, published by the program bus.
//!
//! - [`OnAir`] is the selected source plus the scene name the cut was made
//!   for. Its `seq` grows with EVERY publication: every cut (a cut to the
//!   source already on air too, so a same-scene press is still an event) and
//!   the startup selection.
//! - `ProgramBus` publishes it on a `tokio::sync::watch` inside `cut` (under
//!   its state lock, so in cut order) and inside `select_initial`, always with
//!   `send_modify`. A `send` would DROP the value while nobody subscribed yet,
//!   and `restore_selected_source` runs before any task subscribes.
//! - Every publisher names the scene: a playlist press passes the playlist's
//!   catalog scene (`scene_catalog`, whatever ASCII case was pressed), a
//!   manual press the scene pressed, "OBS manuál" itself none; the startup
//!   restore and a dashboard cut pass the playlist's catalog scene; the OBS
//!   follow the cg OBS scene it follows, but only when it cuts: a followed
//!   change that keeps the source (manual → manual, both -1) publishes
//!   nothing, so the published scene stays the earlier one until L5 deletes
//!   the follow.
//! - [`program_scene_name`] is the ONE name resolver: the scene, else "OBS
//!   manuál" for the NDI input, else none. It never asks cg OBS.
//! - #221 L4b: [`on_air_set`] is the ONE "which playlists are on air" rule,
//!   and [`on_air_changes`] the events a change of it becomes
//!   (`program_authority.rs`, the playback authority).
//!
//! The facade's per-session preview and feedback (#221 L2, L3),
//! `/api/v1/status` and the playback authority (L4b) read them.

use std::collections::BTreeSet;

use sp_core::config::{PROGRAM_INPUT_ID, PROGRAM_INPUT_LABEL};

/// What the program shows, as the bus last published it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OnAir {
    /// Grows by one with every publication (0 = nothing published yet).
    pub seq: u64,
    /// The selected source (a playlist id, or `PROGRAM_INPUT_ID` for the NDI
    /// input "OBS manuál"); `None` before the first selection.
    pub source: Option<i64>,
    /// The scene name the source was selected for; `None` when the publisher
    /// knew none (the resolver then names the NDI input by its label).
    pub scene: Option<String>,
}

impl OnAir {
    /// The publication after this one: `source` on air for `scene`.
    pub fn next(&self, source: i64, scene: Option<&str>) -> Self {
        Self {
            seq: self.seq + 1,
            source: Some(source),
            scene: scene.map(str::to_string),
        }
    }
}

/// The program's scene name: the scene it was cut for, else "OBS manuál"
/// for the NDI input, else none (nothing on air, or a playlist whose catalog
/// names no scene). The one resolver every consumer uses; it never asks cg
/// OBS.
pub fn program_scene_name(on_air: &OnAir) -> Option<String> {
    match (&on_air.scene, on_air.source) {
        (Some(scene), _) => Some(scene.clone()),
        (None, Some(PROGRAM_INPUT_ID)) => Some(PROGRAM_INPUT_LABEL.to_string()),
        _ => None,
    }
}

/// #221 L4b (design record 5873773896 §1e): the playlists on air —
/// `SP-program`'s source when it is a playlist (the NDI input "OBS manuál"
/// is none), together with the playlist SongPlayer last told cg OBS to show
/// (`legacy_cg.shown`). Until B4 step 6 the legacy consumers (Arena, FOH,
/// lv1, strih) still take cg OBS's program, so a playlist cg OBS still shows
/// stays on air: after a dashboard cut to "OBS manuál" (the input carries
/// it), or while a mirror is unanswered or failed.
pub fn on_air_set(on_air: &OnAir, _cg_shown: Option<i64>) -> BTreeSet<i64> {
    let program = on_air.source.filter(|&source| source != PROGRAM_INPUT_ID);
    program.into_iter().collect()
}

/// #221 L4b: the `(playlist, on)` events of a change from `previous` to
/// `current`: OFF for every playlist that left, then ON for EVERY member —
/// the re-kick, so a press of the scene already on air (a new `seq`, the
/// same set) plays a playlist paused out of band. Each part ascending.
pub fn on_air_changes(previous: &BTreeSet<i64>, current: &BTreeSet<i64>) -> Vec<(i64, bool)> {
    let off = previous.difference(current).map(|&pid| (pid, false));
    off.chain(current.iter().map(|&pid| (pid, true))).collect()
}

#[cfg(test)]
#[path = "program_on_air_tests.rs"]
mod tests;
