//! #221 L4b: the program fields of `GET /api/v1/status` come from
//! SongPlayer's OWN program, never from cg OBS's scene detection. In its own
//! file so `api/routes.rs` (1000/1000) does not grow.
//!
//! - `active_scene`: the one scene-name resolver
//!   (`program_on_air::program_scene_name`: the scene the program was cut
//!   for, else "OBS manuál" for the NDI input, else none);
//! - `active_playlist_ids`: the playlists on air
//!   (`program_on_air::on_air_set`: SP-program's playlist ∪ the one cg OBS
//!   was told to show), ascending — the set the playback authority plays.

use crate::playback::program_bus::ProgramBus;
use crate::playback::program_on_air::{on_air_set, program_scene_name};

/// `(active_scene, active_playlist_ids)` of `/api/v1/status` (module doc).
pub fn on_air_fields(bus: &ProgramBus) -> (Option<String>, Vec<i64>) {
    let on_air = bus.on_air_now();
    let playlists = on_air_set(&on_air, bus.legacy_cg().shown_now());
    (program_scene_name(&on_air), playlists.into_iter().collect())
}
