//! The program bus's persisted source (#209): the setting, the one cut
//! path and the startup restore. A child of `program_bus.rs` (1000-line
//! cap); it reaches the bus's `cut_serial`.

use sqlx::SqlitePool;
use tracing::{info, warn};

use super::{ProgramBus, ProgramStatus, SETTING_PROGRAM_SOURCE};
use crate::playback::fleet_shift::timeline_now_100ns;

/// Persist the selected program source (a playlist id).
pub async fn persist_selected_source(pool: &SqlitePool, pid: i64) -> Result<(), sqlx::Error> {
    crate::db::models::set_setting(pool, SETTING_PROGRAM_SOURCE, &pid.to_string()).await
}

/// Persist `pid` as the selected source FIRST, then cut the program to it (a
/// failed write cuts nothing) and publish it as on air for `scene` (#221). The
/// one cut path of `POST /api/v1/program/cut` and the #213 remote control; two
/// cuts never interleave, so the persisted source is always the one on
/// program.
pub async fn persist_and_cut(
    pool: &SqlitePool,
    bus: &ProgramBus,
    pid: i64,
    scene: Option<&str>,
) -> Result<ProgramStatus, sqlx::Error> {
    let _serial = bus.cut_serial.lock().await;
    persist_selected_source(pool, pid).await?;
    Ok(bus.cut(pid, timeline_now_100ns(), scene)) // #224 part 2: the stamps' timeline
}

/// Restore the persisted program source into `bus` (startup). Returns the
/// restored source id; a missing or unreadable setting leaves the program on
/// its standby pair. The #212 NDI input (`PROGRAM_INPUT_ID`) is restored only
/// while it is active (enabled with a source) — otherwise it is not a source.
/// #221: published as on air with the playlist's catalog scene
/// (`scene_catalog::scene_of_source`; none for the input).
pub async fn restore_selected_source(pool: &SqlitePool, bus: &ProgramBus) -> Option<i64> {
    let raw = match crate::db::models::get_setting(pool, SETTING_PROGRAM_SOURCE).await {
        Ok(v) => v?,
        Err(e) => {
            warn!(%e, "program bus: reading the persisted source failed");
            return None;
        }
    };
    let pid = match raw.trim().parse::<i64>() {
        Ok(pid) => pid,
        Err(e) => {
            warn!(%e, raw = %raw, "program bus: persisted source is not a source id");
            return None;
        }
    };
    if pid == sp_core::config::PROGRAM_INPUT_ID && !input_active(pool).await {
        warn!(
            source = pid,
            "program bus: the persisted source is the NDI input, which is disabled or has no source — not restored"
        );
        return None;
    }
    let scene = crate::playback::scene_catalog::scene_of_source(pool, pid).await;
    bus.select_initial(pid, scene.as_deref());
    info!(source = pid, scene = ?scene, "program bus: restored the selected source");
    Some(pid)
}

/// #212: whether the stored settings make the NDI input a source (enabled
/// with a source name; an unreadable setting counts as not).
async fn input_active(pool: &SqlitePool) -> bool {
    crate::playback::ndi_input::load_input_settings(pool)
        .await
        .is_ok_and(|s| s.active())
}
