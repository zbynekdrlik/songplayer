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
//! - Every publisher names the scene: a press passes the scene pressed, the
//!   startup restore and a dashboard cut the playlist's catalog scene
//!   (`scene_catalog`), the OBS follow the cg OBS scene it follows.
//! - [`program_scene_name`] is the ONE name resolver: the scene, else "OBS
//!   manuál" for the NDI input, else none. It never asks cg OBS.
//!
//! The consumers (the facade's feedback, `/api/v1/status`, the playback
//! authority) arrive in the later lanes of #221.

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

#[cfg(test)]
#[path = "program_on_air_tests.rs"]
mod tests;
