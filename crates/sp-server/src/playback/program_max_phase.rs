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
//! readback, `ndi_feed.rs`), with the queue's rules it drives. #251
//! (19.10.2026): so is `program_max_spout_send`, how both senders hand each
//! frame to Spout (the worker gives it to them before every send), and
//! `program_max_size`, the size of MAX's target (the worker makes the
//! target and its sender anew at a changed size).

use std::time::Duration;

use sp_core::config::{
    NdiSource, SETTING_PROGRAM_MAX_COMPOSE_PHASE_MS, SETTING_PROGRAM_MAX_SPOUT_SEND,
    SETTING_PROGRAM_MAX_VBLANK_PHASE_MS, SETTING_PROGRAM_NDI_SOURCE,
    SETTING_PROGRAM_VIDEO_QUIET_FROM_MS, SETTING_PROGRAM_VIDEO_QUIET_TO_MS, SpoutSend,
    program_max_compose_phase_us, program_max_spout_send, program_max_vblank_phase_us,
    program_ndi_source, program_video_quiet_from_us, program_video_quiet_to_us,
};
use sp_gpu::{CANVAS_HEIGHT, CANVAS_WIDTH};
use sqlx::SqlitePool;
use tracing::{info, warn};

use super::{MaxOut, Queue, fhd_off_reason};
use crate::playback::ndi_feed::NdiStatus;

/// #251 (19.10.2026): the size of `SP-program-MAX`'s target, `WxH`.
/// Measured on SNV: while Arena receives the 3840×2160 MAX, its own 30 fps
/// clip lost 38–58 % of its frames on the wall, the same with every Spout
/// hand-off (`program_max_spout_send`); Arena's composition is 2580×360.
/// Read with MAX's settings every 5 s, no restart; for the wall's A/B.
pub const SETTING_PROGRAM_MAX_SIZE: &str = "program_max_size";

/// MAX's size unless the setting names another: the canvas, 3840×2160.
pub const DEFAULT_MAX_SIZE: (u32, u32) = (CANVAS_WIDTH, CANVAS_HEIGHT);

/// #251: a stored `program_max_size`: `WxH` (trimmed), each side even, the
/// width 640..=3840 and the height 360..=2160; anything else, absent
/// included, is [`DEFAULT_MAX_SIZE`], so a deploy changes nothing.
pub fn program_max_size(raw: Option<&str>) -> (u32, u32) {
    let size = raw
        .and_then(|raw| raw.trim().split_once('x'))
        .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)));
    match size {
        Some((w, h))
            if (640..=CANVAS_WIDTH).contains(&w)
                && (360..=CANVAS_HEIGHT).contains(&h)
                && w.is_multiple_of(2)
                && h.is_multiple_of(2) =>
        {
            (w, h)
        }
        _ => DEFAULT_MAX_SIZE,
    }
}

/// #279 P1b: who wants the compositor, from ONE read of the queue
/// (`MaxOut::wants`): MAX's output (its setting), the FHD Spout sender (its
/// setting AND MAX on, `fhd_off_reason`), NDI's readback
/// (`program_ndi_source` = `gpu`). The queue's acceptance, its release and
/// the worker read this one rule.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Wants {
    pub max: bool,
    pub fhd: bool,
    pub ndi: bool,
}

impl Wants {
    /// Whether anything wants the compositor: the thread takes jobs; with
    /// nothing, it releases the GPU.
    pub fn any(self) -> bool {
        self.max || self.fhd || self.ndi
    }
}

impl Queue {
    /// Who wants the compositor now ([`Wants`]).
    pub(super) fn wants(&self) -> Wants {
        Wants {
            max: self.enabled,
            fhd: fhd_off_reason(self.fhd_enabled, self.enabled).is_none(),
            ndi: self.ndi == NdiSource::Gpu,
        }
    }
}

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
            if !queue.wants().any() {
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

    /// #251: apply `program_max_spout_send`; returns whether it changed.
    pub fn set_spout_send(&self, mode: SpoutSend) -> bool {
        let mut queue = self.lock_queue();
        let changed = queue.spout_send != mode;
        queue.spout_send = mode;
        changed
    }

    /// #251: `program_max_spout_send`, as last applied.
    pub fn spout_send(&self) -> SpoutSend {
        self.lock_queue().spout_send
    }

    /// #251: apply `program_max_size`; returns whether it changed.
    pub fn set_max_size(&self, size: (u32, u32)) -> bool {
        let mut queue = self.lock_queue();
        let changed = queue.max_size != size;
        queue.max_size = size;
        changed
    }

    /// #251: `program_max_size`, as last applied.
    pub fn max_size(&self) -> (u32, u32) {
        self.lock_queue().max_size
    }

    /// #279 P1b: who wants the compositor ([`Wants`]), read under one lock.
    pub fn wants(&self) -> Wants {
        self.lock_queue().wants()
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

/// #251: read `program_max_spout_send` (`sp_core::config`'s rule).
pub async fn load_spout_send(pool: &SqlitePool) -> Result<SpoutSend, sqlx::Error> {
    let raw = crate::db::models::get_setting(pool, SETTING_PROGRAM_MAX_SPOUT_SEND).await?;
    Ok(program_max_spout_send(raw.as_deref()))
}

/// #251: read `program_max_size` ([`program_max_size`]'s rule).
pub async fn load_max_size(pool: &SqlitePool) -> Result<(u32, u32), sqlx::Error> {
    let raw = crate::db::models::get_setting(pool, SETTING_PROGRAM_MAX_SIZE).await?;
    Ok(program_max_size(raw.as_deref()))
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
    match load_spout_send(pool).await {
        Ok(mode) => {
            if max.set_spout_send(mode) {
                info!(mode = mode.as_str(), "program max: Spout hand-off applied");
            }
        }
        Err(e) => warn!(%e, "program max: reading the Spout hand-off setting failed"),
    }
    match load_max_size(pool).await {
        Ok((width, height)) => {
            if max.set_max_size((width, height)) {
                info!(width, height, "program max: size applied");
            }
        }
        Err(e) => warn!(%e, "program max: reading the size setting failed"),
    }
}

#[cfg(test)]
#[path = "program_max_tests_ndi.rs"]
mod tests_ndi;
