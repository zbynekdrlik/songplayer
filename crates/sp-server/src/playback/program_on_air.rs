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
//!   restore and a dashboard cut pass the playlist's catalog scene.
//! - [`program_scene_name`] is the ONE name resolver: the scene, else "OBS
//!   manuál" for the NDI input, else none. It never asks cg OBS.
//! - #221 L4b: [`on_air_set`] is the ONE "which playlists are on air" rule,
//!   and [`on_air_changes`] the events a change of it becomes
//!   (`program_authority.rs`, the playback authority).
//! - #221 (release 0.69.0 review 🟡 2): [`wall_owner`] is the ONE playlist
//!   that writes the shared wall outputs; the authority publishes it with
//!   the set. #221 B4 step 6: the set is that playlist alone (no more cg OBS
//!   record), so it has at most one member.
//!
//! The facade's per-session preview and feedback (#221 L2, L3),
//! `/api/v1/status` and the playback authority (L4b) read them.

use std::collections::BTreeSet;

use sp_core::config::{
    PROGRAM_BLANK_ID, PROGRAM_BLANK_LABEL, PROGRAM_INPUT_ID, PROGRAM_INPUT_LABEL,
};

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
        (None, Some(PROGRAM_BLANK_ID)) => Some(PROGRAM_BLANK_LABEL.to_string()),
        _ => None,
    }
}

/// #221 (release 0.69.0 review 🟡 2; B4 step 6): the ONE playlist that
/// owns the shared wall outputs — `#sp-subs*` (`ShowSubtitles`), the
/// `#sp-title` clip (the title timers, a re-sync's title) and the Presenter
/// stage display: `SP-program`'s source when it is a playlist, else none (the
/// NDI input "OBS manuál" and Blank (#245) name no playlist, and nothing
/// selected yet). B4
/// step 6 deleted the legacy mirror and SongPlayer's record of what it told
/// cg OBS, so cg OBS's program owns nothing any more.
pub fn wall_owner(on_air: &OnAir) -> Option<i64> {
    on_air
        .source
        .filter(|&source| source != PROGRAM_INPUT_ID && source != PROGRAM_BLANK_ID)
}

/// #221 L4b (design record 5873773896 §1e; B4 step 6): the playlists on air
/// — the wall owner, `SP-program`'s playlist, alone. Every consumer takes
/// `SP-program` now, so a playlist off it plays for nobody. At most one.
pub fn on_air_set(on_air: &OnAir) -> BTreeSet<i64> {
    wall_owner(on_air).into_iter().collect()
}

/// #221 L4b: the `(playlist, on)` events of a change from `previous` to
/// `current`: OFF for every playlist that left, then ON for every playlist
/// that entered and for `cut_to` — the source `SP-program` was just cut to
/// (a new publication) — even when it was on air already: the re-kick, so
/// a press of the scene already on air plays a playlist paused out of band.
/// A member nobody cut to is never re-kicked, so a paused one stays paused
/// (review round 1). Each part ascending.
pub fn on_air_changes(
    previous: &BTreeSet<i64>,
    current: &BTreeSet<i64>,
    cut_to: Option<i64>,
) -> Vec<(i64, bool)> {
    let off = previous.difference(current).map(|&pid| (pid, false));
    let on = current
        .iter()
        .filter(|&&pid| !previous.contains(&pid) || Some(pid) == cut_to)
        .map(|&pid| (pid, true));
    off.chain(on).collect()
}

#[cfg(test)]
#[path = "program_on_air_tests.rs"]
mod tests;
