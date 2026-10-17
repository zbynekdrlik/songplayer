//! The `program-max` thread's two phases on the primary display's refresh
//! grid (`program_max_vblank.rs`), a child module of `program_max.rs` (it
//! reads the hand-off's private queue): where after each vblank a
//! `SP-program-MAX` send starts (#223 follow-up, the setting
//! `program_max_vblank_phase_ms`) and (#251) where its compose starts
//! (`program_max_compose_phase_ms`). Both are read with MAX's other
//! settings, before the thread starts and every `MAX_SETTINGS_POLL` after
//! (`apply_phase_settings`), and take effect at the next boundary. #251
//! step 4: so is the `SP-program` video side's quiet window
//! (`program_video_quiet_from_ms` / `_to_ms`, `program_video_quiet.rs`).
//! #279 P1b: so is `program_ndi_source` (NDI `SP-program` from the GPU
//! readback, `ndi_feed.rs`), with the queue's rules it drives.

use std::time::Duration;

use sp_core::config::{
    NdiSource, SETTING_PROGRAM_MAX_COMPOSE_PHASE_MS, SETTING_PROGRAM_MAX_VBLANK_PHASE_MS,
    SETTING_PROGRAM_NDI_SOURCE, SETTING_PROGRAM_VIDEO_QUIET_FROM_MS,
    SETTING_PROGRAM_VIDEO_QUIET_TO_MS, program_max_compose_phase_us, program_max_vblank_phase_us,
    program_ndi_source, program_video_quiet_from_us, program_video_quiet_to_us,
};
use sqlx::SqlitePool;
use tracing::{info, warn};

use super::MaxOut;
use crate::playback::ndi_feed::NdiStatus;

impl MaxOut {
    /// Apply `program_max_vblank_phase_ms` (µs); returns whether it changed.
    pub fn set_vblank_phase_us(&self, us: u64) -> bool {
        let mut queue = self.lock_queue();
        let changed = queue.vblank_phase_us != us;
        queue.vblank_phase_us = us;
        changed
    }

    /// Where after each vblank a send starts (the setting).
    pub fn vblank_phase(&self) -> Duration {
        Duration::from_micros(self.lock_queue().vblank_phase_us)
    }

    /// #251: apply `program_max_compose_phase_ms` (µs); returns whether it
    /// changed.
    pub fn set_compose_phase_us(&self, us: u64) -> bool {
        let mut queue = self.lock_queue();
        let changed = queue.compose_phase_us != us;
        queue.compose_phase_us = us;
        changed
    }

    /// #251: where after each vblank a compose starts (the setting).
    pub fn compose_phase(&self) -> Duration {
        Duration::from_micros(self.lock_queue().compose_phase_us)
    }

    /// #279 P1b: apply `program_ndi_source`; returns whether it changed.
    /// When nothing wants the compositor any more the queued jobs go, and
    /// the thread is woken either way (it releases what nothing wants).
    pub fn set_ndi_source(&self, source: NdiSource) -> bool {
        let changed = {
            let mut queue = self.lock_queue();
            let changed = queue.ndi != source;
            queue.ndi = source;
            if !queue.wanted() {
                while queue.jobs.take().is_some() {}
            }
            changed
        };
        self.ready.notify_all();
        changed
    }

    /// #279 P1b: `program_ndi_source`, as last applied.
    pub fn ndi_source(&self) -> NdiSource {
        self.lock_queue().ndi
    }

    /// #279 P1b: whether the thread reads NDI's picture back.
    pub fn ndi_wanted(&self) -> bool {
        self.ndi_source() == NdiSource::Gpu
    }

    /// #279 P1b: the `ndi` block of `GET /api/v1/program`.
    pub fn ndi_status(&self) -> NdiStatus {
        self.ndi.status(self.ndi_source())
    }
}

/// #279 P1b: read `program_ndi_source` (`sp_core::config`'s rule).
pub async fn load_ndi_source(pool: &SqlitePool) -> Result<NdiSource, sqlx::Error> {
    let raw = crate::db::models::get_setting(pool, SETTING_PROGRAM_NDI_SOURCE).await?;
    Ok(program_ndi_source(raw.as_deref()))
}

/// Read `program_max_vblank_phase_ms`, µs (`sp_core::config`'s rule).
pub async fn load_max_vblank_phase_us(pool: &SqlitePool) -> Result<u64, sqlx::Error> {
    let raw = crate::db::models::get_setting(pool, SETTING_PROGRAM_MAX_VBLANK_PHASE_MS).await?;
    Ok(program_max_vblank_phase_us(raw.as_deref()))
}

/// #251: read `program_max_compose_phase_ms`, µs (`sp_core::config`'s rule).
pub async fn load_max_compose_phase_us(pool: &SqlitePool) -> Result<u64, sqlx::Error> {
    let raw = crate::db::models::get_setting(pool, SETTING_PROGRAM_MAX_COMPOSE_PHASE_MS).await?;
    Ok(program_max_compose_phase_us(raw.as_deref()))
}

/// #251 step 4: read the video side's quiet window, µs (`sp_core::config`'s
/// rule for each end).
pub async fn load_video_quiet_us(pool: &SqlitePool) -> Result<(u64, u64), sqlx::Error> {
    let from = crate::db::models::get_setting(pool, SETTING_PROGRAM_VIDEO_QUIET_FROM_MS).await?;
    let to = crate::db::models::get_setting(pool, SETTING_PROGRAM_VIDEO_QUIET_TO_MS).await?;
    Ok((
        program_video_quiet_from_us(from.as_deref()),
        program_video_quiet_to_us(to.as_deref()),
    ))
}

/// Apply both stored phases and the video side's quiet window to `max` (an
/// unreadable one changes nothing).
pub(super) async fn apply_phase_settings(pool: &SqlitePool, max: &MaxOut) {
    match load_max_vblank_phase_us(pool).await {
        Ok(us) => {
            if max.set_vblank_phase_us(us) {
                info!(phase_us = us, "program max: vblank phase applied");
            }
        }
        Err(e) => warn!(%e, "program max: reading the vblank phase failed"),
    }
    match load_max_compose_phase_us(pool).await {
        Ok(us) => {
            if max.set_compose_phase_us(us) {
                info!(phase_us = us, "program max: compose phase applied");
            }
        }
        Err(e) => warn!(%e, "program max: reading the compose phase failed"),
    }
    match load_video_quiet_us(pool).await {
        Ok((from_us, to_us)) => {
            if max.video.set_window_us(from_us, to_us) {
                info!(from_us, to_us, "program output: video quiet window applied");
            }
        }
        Err(e) => warn!(%e, "program output: reading the video quiet window failed"),
    }
    match load_ndi_source(pool).await {
        Ok(source) => {
            if max.set_ndi_source(source) {
                info!(
                    source = source.as_str(),
                    "program output: NDI source setting applied"
                );
            }
        }
        Err(e) => warn!(%e, "program output: reading the NDI source setting failed"),
    }
}

#[cfg(test)]
#[path = "program_max_tests_ndi.rs"]
mod tests_ndi;
