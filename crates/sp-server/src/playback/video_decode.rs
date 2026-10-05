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
//!   song keeps its reader. The SDK-clocked path (`genlock_pacing` off, not
//!   used on the box) stays software.
//! - `GET /api/v1/status` → `video_decode` ([`status`]): the setting and the
//!   process's `sp_decoder::hw_counters` (every reader opened in `Hardware`
//!   mode, the decode bench's included).
//!
//! The reader decides the rest (`sp_decoder::hw_decode`): a hardware open
//! that fails, or a decode error on the GPU path, falls back to software for
//! that file with a WARN — never a dead song.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sp_core::config::{DEFAULT_VIDEO_HW_DECODE, SETTING_VIDEO_HW_DECODE, video_hw_decode};
use sp_decoder::{DecodeMode, HwDecodeStats};
use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tracing::{info, warn};

/// How often the settings task re-reads `video_hw_decode` (the cadence of
/// the other settings tasks: MAX, VBAN, the NDI input).
pub const VIDEO_DECODE_SETTINGS_POLL: Duration = Duration::from_secs(5);

/// The process's `video_hw_decode`, as last applied.
#[derive(Debug)]
pub struct HwDecodeSetting {
    hw: AtomicBool,
}

impl Default for HwDecodeSetting {
    fn default() -> Self {
        HwDecodeSetting {
            hw: AtomicBool::new(DEFAULT_VIDEO_HW_DECODE),
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

/// Apply the stored setting to `setting` (an unreadable one changes
/// nothing).
pub async fn apply_setting(pool: &SqlitePool, setting: &HwDecodeSetting) {
    match load_hw_decode(pool).await {
        Ok(on) => {
            if setting.set(on) {
                info!(
                    hw_decode = on,
                    "video decode: setting applied (from the next song)"
                );
            }
        }
        Err(e) => warn!(%e, "video decode: reading the setting failed"),
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
            _ = tokio::time::sleep(poll) => apply_setting(&pool, &setting).await,
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
        "video decode: the setting at startup"
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
    /// Files opened in `Hardware` mode.
    pub hw_requested: u64,
    /// Of those, the ones whose first picture came out of the GPU decoder.
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
    }
}

/// The process's status (the global setting and `sp_decoder::hw_counters`).
pub fn status() -> VideoDecodeStatus {
    status_of(&global(), sp_decoder::hw_counters().snapshot())
}

#[cfg(test)]
#[path = "video_decode_tests.rs"]
mod tests;
