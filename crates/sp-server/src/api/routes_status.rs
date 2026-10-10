//! #221 L4b: the program fields of `GET /api/v1/status` come from
//! SongPlayer's OWN program (#221 L6 deleted cg OBS's scene detection). In its own
//! file because `api/routes.rs` sits at the 1000-line cap.
//!
//! - `active_scene`: the one scene-name resolver
//!   (`program_on_air::program_scene_name`: the scene the program was cut
//!   for, else "OBS manuál" for the NDI input, else none);
//! - `active_playlist_ids`: the playlists on air
//!   (`program_on_air::on_air_set`: SP-program's playlist alone, #221 B4
//!   step 6; none while "OBS manuál" is on program) — the set the playback
//!   authority plays.
//!
//! #229 item C: the node's name (`node_name`) is read here too.
//!
//! #136: `HeavyContainmentStatus` lives here too (re-exported by `routes`),
//! which made room in `routes.rs` for `status.metadata`; #223 S3b moved
//! `ToolsStatusResponse` here too, for `status.video_decode`.

use serde::{Deserialize, Serialize};

use crate::playback::program_bus::ProgramBus;
use crate::playback::program_on_air::{on_air_set, program_scene_name};

/// #203: the containment applied to the heavy children, surfaced on `/status` so
/// the dashboard health + the next box measurement can read the effective cap.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct HeavyContainmentStatus {
    /// Job Object CPU hard-cap, percent of TOTAL machine CPU time.
    pub cap_pct: u8,
    /// Job Object affinity mask (lowercase hex, no `0x`) — the cores the heavy
    /// children may run on.
    pub affinity_mask: String,
    /// SongPlayer's own scheduling priority class (`high` on the Windows box).
    pub priority_class: String,
}

/// The tools block of `/api/v1/status` (yt-dlp, FFmpeg, the JS runtime).
#[derive(Debug, Serialize, Deserialize)]
pub struct ToolsStatusResponse {
    pub ytdlp_available: bool,
    pub ffmpeg_available: bool,
    pub ytdlp_version: Option<String>,
    #[serde(default)]
    pub js_runtime_ok: bool,
    #[serde(default)]
    pub deno_version: Option<String>,
}

/// #229 item C: this node's exchange name for `/api/v1/status` (the health
/// bar's node segment): the trimmed `node_name`, `None` when it is unset or
/// blank (the exchange off) or cannot be read.
pub async fn node_name(pool: &sqlx::SqlitePool) -> Option<String> {
    crate::db::models::get_setting(pool, sp_core::config::SETTING_NODE_NAME)
        .await
        .ok()
        .flatten()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

/// `(active_scene, active_playlist_ids)` of `/api/v1/status` (module doc).
pub fn on_air_fields(bus: &ProgramBus) -> (Option<String>, Vec<i64>) {
    let on_air = bus.on_air_now();
    let playlists = on_air_set(&on_air);
    (program_scene_name(&on_air), playlists.into_iter().collect())
}

/// #230: `GET /api/v1/background-hold` — the hold the health bar shows
/// (`crate::background_hold::status`).
pub async fn background_hold(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
) -> axum::Json<sp_core::background_hold::BackgroundHold> {
    axum::Json(crate::background_hold::status(&state.pool).await)
}

#[cfg(test)]
#[path = "routes_status_tests.rs"]
mod tests;
