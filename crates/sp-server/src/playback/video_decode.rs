//! #223 S3b: which Media Foundation decode path a song opens on, and what
//! the hardware path did.
//!
//! - The setting `video_hw_decode` (`sp_core::config::video_hw_decode`: ON
//!   only for `"true"`, OFF by default until the main session's box gate
//!   passes) is applied to the process value ([`HwDecodeSetting`]) before
//!   any pipeline opens a song ([`start`], from `lib.rs`), then every
//!   [`VIDEO_DECODE_SETTINGS_POLL`] by [`run_settings_task`].
//! - The paced decode producer (`pipeline_paced.rs`) reads [`global`]'s
//!   [`HwDecodeSetting::mode`] when it OPENS a song, so a change applies from
//!   the next song (one opened at most one poll after the save); a playing
//!   song keeps its reader (the only decode path since #221 lane 3).
//! - #282: the same task applies the decode cap — `max_resolution` through
//!   the download's own rule (`downloader::format::max_resolution`: auto =
//!   2160 with hardware decode, else 1440) — to [`HwDecodeSetting::max_height`];
//!   the producer opens each song at it, so a picture larger than the cap's
//!   16:9 box is scaled at decode (`sp_decoder::hw_decode::scaled_size`).
//! - `GET /api/v1/status` → `video_decode` ([`status`]): the setting, the cap
//!   and the process's `sp_decoder::hw_counters` (every reader opened in
//!   `Hardware` mode, the decode bench's included; the scaled opens of any
//!   mode).
//!
//! The reader decides the rest (`sp_decoder::hw_decode`): a hardware open
//! that fails, or a decode error on the GPU path, falls back to software for
//! that file with a WARN — never a dead song.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sp_core::config::{
    DEFAULT_VIDEO_HW_DECODE, SETTING_MAX_RESOLUTION, SETTING_VIDEO_HW_DECODE, video_hw_decode,
};
use sp_decoder::{DecodeMode, HwDecodeStats};
use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::downloader::format::max_resolution;

/// How often the settings task re-reads `video_hw_decode` and
/// `max_resolution` (the cadence of the other settings tasks: MAX, VBAN,
/// the NDI input).
pub const VIDEO_DECODE_SETTINGS_POLL: Duration = Duration::from_secs(5);

/// The process's `video_hw_decode` and decode cap, as last applied.
#[derive(Debug)]
pub struct HwDecodeSetting {
    hw: AtomicBool,
    /// #282: the decode cap (rows), `max_resolution`'s rule.
    max_height: AtomicU32,
}

impl Default for HwDecodeSetting {
    fn default() -> Self {
        HwDecodeSetting {
            hw: AtomicBool::new(DEFAULT_VIDEO_HW_DECODE),
            max_height: AtomicU32::new(max_resolution(None, DEFAULT_VIDEO_HW_DECODE)),
        }
    }
}

impl HwDecodeSetting {
    /// Whether hardware decode is on.
    pub fn hw(&self) -> bool {
        self.hw.load(Ordering::Relaxed)
    }

    /// The mode a song opened now decodes in.
    pub fn mode(&self) -> DecodeMode {
        DecodeMode::from_hw_flag(self.hw())
    }

    /// Apply `on`; whether it changed.
    pub fn set(&self, on: bool) -> bool {
        self.hw.swap(on, Ordering::Relaxed) != on
    }

    /// #282: the decode cap a song opened now is decoded under (rows: a
    /// picture larger than its 16:9 box is scaled into it).
    pub fn max_height(&self) -> u32 {
        self.max_height.load(Ordering::Relaxed)
    }

    /// #282: apply the decode cap `rows`; whether it changed.
    pub fn set_max_height(&self, rows: u32) -> bool {
        self.max_height.swap(rows, Ordering::Relaxed) != rows
    }
}

static GLOBAL: OnceLock<Arc<HwDecodeSetting>> = OnceLock::new();

/// The process's setting (the paced producer reads it at each song's open).
pub fn global() -> Arc<HwDecodeSetting> {
    GLOBAL.get_or_init(Arc::default).clone()
}

/// Read `video_hw_decode` (ON only for `"true"`).
pub async fn load_hw_decode(pool: &SqlitePool) -> Result<bool, sqlx::Error> {
    let raw = crate::db::models::get_setting(pool, SETTING_VIDEO_HW_DECODE).await?;
    Ok(video_hw_decode(raw.as_deref()))
}

/// #282: the decode cap from the stored `max_resolution` and `hw`
/// (`video_hw_decode`): the download's own rule.
pub async fn load_max_height(pool: &SqlitePool, hw: bool) -> Result<u32, sqlx::Error> {
    let raw = crate::db::models::get_setting(pool, SETTING_MAX_RESOLUTION).await?;
    Ok(max_resolution(raw.as_deref(), hw))
}

/// `video_hw_decode` and the decode cap it gives `max_resolution`.
async fn load_settings(pool: &SqlitePool) -> Result<(bool, u32), sqlx::Error> {
    let on = load_hw_decode(pool).await?;
    Ok((on, load_max_height(pool, on).await?))
}

/// Apply the stored settings to `setting`; whether anything changed. An
/// unreadable one changes nothing.
pub async fn apply_setting(pool: &SqlitePool, setting: &HwDecodeSetting) -> bool {
    match load_settings(pool).await {
        Ok((on, max_height)) => {
            let hw_changed = setting.set(on);
            let cap_changed = setting.set_max_height(max_height);
            let changed = hw_changed || cap_changed;
            if changed {
                info!(
                    hw_decode = on,
                    max_height, "video decode: settings applied (from the next song)"
                );
            }
            changed
        }
        Err(e) => {
            warn!(%e, "video decode: reading the settings failed");
            false
        }
    }
}

/// Re-read the setting every `poll` and apply a change, until shutdown.
pub async fn run_settings_task(
    pool: SqlitePool,
    setting: Arc<HwDecodeSetting>,
    mut shutdown: broadcast::Receiver<()>,
    poll: Duration,
) {
    loop {
        tokio::select! {
            _ = shutdown.recv() => break,
            _ = tokio::time::sleep(poll) => {
                apply_setting(&pool, &setting).await;
            }
        }
    }
    info!("video decode: settings task stopped");
}

/// Apply the stored setting to the process value BEFORE any pipeline opens
/// a song, then start its settings task (`lib.rs`, before the pipelines).
pub async fn start(pool: &SqlitePool, shutdown: &broadcast::Sender<()>) {
    let setting = global();
    apply_setting(pool, &setting).await;
    info!(
        hw_decode = setting.hw(),
        max_height = setting.max_height(),
        "video decode: the settings at startup"
    );
    tokio::spawn(run_settings_task(
        pool.clone(),
        setting,
        shutdown.subscribe(),
        VIDEO_DECODE_SETTINGS_POLL,
    ));
}

/// `GET /api/v1/status` → `video_decode`. Every count runs since the process
/// started (files, not live state). A missing key deserializes to its zero
/// value (older clients / the mock stay ok).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoDecodeStatus {
    /// The setting `video_hw_decode`, as last applied.
    pub hw_decode: bool,
    /// Opens asked for in `Hardware` mode (a file that then opened nowhere
    /// counts here and in none of the counts below).
    pub hw_requested: u64,
    /// Of those, the ones whose first picture came out of the GPU decoder
    /// (#282: a file opened scaled to the cap counts in neither this nor
    /// `mf_software`; its decoder is not visible behind the video
    /// processor).
    pub gpu_decodes: u64,
    /// Of those, the ones on the GPU path that Media Foundation's decoder
    /// decoded in software (no decoder on the GPU for the stream).
    pub mf_software: u64,
    /// Of those, the ones whose hardware open failed (decoded in software).
    pub open_fallbacks: u64,
    /// Reopens in software after a decode error on the GPU path.
    pub mid_stream_fallbacks: u64,
    /// Changes of decode path mid-file on the GPU path with no error (Media
    /// Foundation's decoder changed its mind).
    pub path_changes: u64,
    /// The last fall back: `"open: …"` / `"mid-stream: …"`.
    pub last_fallback: Option<String>,
    /// #282: the decode cap in force (rows; `max_resolution`'s rule): a
    /// picture larger than its 16:9 box opens scaled into it.
    pub max_height: u32,
    /// #282: files opened scaled to the cap (any mode).
    pub scaled_opens: u64,
    /// #282: scaled opens Media Foundation refused (those files played at
    /// their own size).
    pub scale_refusals: u64,
}

/// The status of `setting` and `stats`.
pub fn status_of(setting: &HwDecodeSetting, stats: HwDecodeStats) -> VideoDecodeStatus {
    VideoDecodeStatus {
        hw_decode: setting.hw(),
        hw_requested: stats.requested,
        gpu_decodes: stats.gpu_decodes,
        mf_software: stats.mf_software,
        open_fallbacks: stats.open_fallbacks,
        mid_stream_fallbacks: stats.mid_stream_fallbacks,
        path_changes: stats.path_changes,
        last_fallback: stats.last_fallback,
        max_height: setting.max_height(),
        scaled_opens: stats.scaled_opens,
        scale_refusals: stats.scale_refusals,
    }
}

/// The process's status (the global setting and `sp_decoder::hw_counters`).
pub fn status() -> VideoDecodeStatus {
    status_of(&global(), sp_decoder::hw_counters().snapshot())
}

#[cfg(test)]
#[path = "video_decode_tests.rs"]
mod tests;
