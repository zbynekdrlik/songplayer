//! Background karaoke stem-separation worker (#14).
//!
//! Mirrors the lyrics worker: a periodic tick that, ONLY while the wall is idle
//! (#154 gate — reused verbatim), separates the next normalized song into its
//! vocals + instrumental sidecars via `scripts/stem_worker.py`. Runs at lowest
//! priority (after lyrics), backoff-gated so a broken row never hot-loops.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sqlx::SqlitePool;
use tokio::sync::{RwLock, broadcast};
use tracing::{error, info, warn};

use crate::lyrics::aligner::isolation_timeout;
use crate::lyrics::idle_gate::{GateLog, gate_setting_enabled, should_defer, wall_activity_from};

/// Songs longer than this are not separated (huge stems, slow); marked terminal.
const STEM_MAX_DURATION_MS: i64 = 30 * 60 * 1000;

/// How often the worker looks for the next song to separate.
const TICK: Duration = Duration::from_secs(10);

pub struct StemWorker {
    pool: SqlitePool,
    tools_dir: PathBuf,
    script_path: PathBuf,
    models_dir: PathBuf,
    ndi_health_registry: Option<Arc<crate::playback::ndi_health::NdiHealthRegistry>>,
    obs_state: Option<Arc<RwLock<crate::obs::ObsState>>>,
    wall_gate_log: std::sync::Mutex<GateLog>,
    /// Logged once when python is unavailable so the operator sees why nothing
    /// is separating, without spamming every tick.
    warned_no_python: std::sync::atomic::AtomicBool,
}

/// Parse the `stem_worker_enabled` setting. Default ON so a fresh deploy starts
/// separating (idle-gated); `false`/`0`/`off`/`no` disable it. Mirrors
/// `lyrics_worker_enabled`.
pub(crate) fn worker_enabled(raw: Option<&str>) -> bool {
    match raw {
        None => true,
        Some(v) => {
            let v = v.trim().to_ascii_lowercase();
            !(v == "false" || v == "0" || v == "off" || v == "no")
        }
    }
}

impl StemWorker {
    pub fn new(
        pool: SqlitePool,
        tools_dir: PathBuf,
        ndi_health_registry: Arc<crate::playback::ndi_health::NdiHealthRegistry>,
        obs_state: Arc<RwLock<crate::obs::ObsState>>,
    ) -> Self {
        let script_path = tools_dir.join("stem_worker.py");
        let models_dir = tools_dir.join("hf_models");
        Self {
            pool,
            tools_dir,
            script_path,
            models_dir,
            ndi_health_registry: Some(ndi_health_registry),
            obs_state: Some(obs_state),
            wall_gate_log: std::sync::Mutex::new(GateLog::default()),
            warned_no_python: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub async fn run(self, mut shutdown_rx: broadcast::Receiver<()>) {
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        info!("stem worker started");
        loop {
            tokio::select! {
                _ = interval.tick() => self.process_next().await,
                _ = shutdown_rx.recv() => {
                    info!("stem worker shutting down");
                    break;
                }
            }
        }
    }

    async fn process_next(&self) {
        // Operational kill-switch, read live each tick.
        let enabled = crate::db::models::get_setting(&self.pool, "stem_worker_enabled")
            .await
            .ok()
            .flatten();
        if !worker_enabled(enabled.as_deref()) {
            return;
        }

        let python = crate::lyrics::bootstrap::venv_python_path(&self.tools_dir);
        if !python.exists() {
            if !self
                .warned_no_python
                .swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                warn!(
                    "stem worker: lyrics venv python not found at {python:?} — karaoke stems wait for the lyrics bootstrap"
                );
            }
            return;
        }

        // #154 idle gate (reused): no heavy GPU separation while the wall is in
        // use. Same `lyrics_gate_when_playing` setting as the lyrics worker.
        let gate_enabled = gate_setting_enabled(
            crate::db::models::get_setting(&self.pool, "lyrics_gate_when_playing")
                .await
                .ok()
                .flatten()
                .as_deref(),
        );
        let activity =
            wall_activity_from(self.ndi_health_registry.as_ref(), self.obs_state.as_ref()).await;
        // Idle-settle hysteresis (2026-09-14 incident): a single idle sample is
        // not enough — resume only after the wall has read idle continuously for
        // `WALL_IDLE_SETTLE`. Shared with the lyrics worker via `GateLog`.
        let now = Instant::now();
        let defer = match self.wall_gate_log.lock() {
            Ok(mut g) => g.defer_settled(gate_enabled, activity, now),
            Err(_) => should_defer(gate_enabled, activity),
        };
        if defer {
            // When deferring only because the settle window has not elapsed (the
            // wall is idle right now), say so instead of "wall in use".
            let detail = if activity.in_use() {
                activity.reason().unwrap_or("wall in use")
            } else {
                "wall just went idle — settling"
            };
            if let Ok(mut g) = self.wall_gate_log.lock()
                && let Some(line) = g.note(true, detail)
            {
                info!("stem_worker: {line}");
            }
            return;
        }
        if let Ok(mut g) = self.wall_gate_log.lock()
            && let Some(line) = g.note(false, "")
        {
            info!("stem_worker: {line}");
        }

        let job = match crate::db::models_stems::get_next_video_for_stems(&self.pool).await {
            Ok(Some(j)) => j,
            Ok(None) => return, // nothing to separate
            Err(e) => {
                warn!(%e, "stem worker: selection query failed");
                return;
            }
        };

        // Terminal skip: a song too long for a sane stem pass.
        if let Some(ms) = job.duration_ms
            && ms > STEM_MAX_DURATION_MS
        {
            warn!(
                video_id = job.video_id,
                duration_ms = ms,
                "stem worker: song exceeds stem duration cap — marking unsupported"
            );
            let _ = crate::db::models_stems::mark_stems_unsupported(&self.pool, job.video_id).await;
            return;
        }

        let audio_path = PathBuf::from(&job.audio_file_path);
        let (vocals_out, instrumental_out) = crate::stems::stem_paths(&audio_path);
        let gpu_mem = crate::db::models::get_setting(&self.pool, "lyrics_gpu_mem_fraction")
            .await
            .ok()
            .flatten();
        let timeout = isolation_timeout(job.duration_ms);

        info!(
            video_id = job.video_id,
            youtube_id = %job.youtube_id,
            song = job.song.as_deref().unwrap_or(""),
            "stem worker: separating"
        );

        let script_path = match self.ensure_script().await {
            Ok(p) => p,
            Err(e) => {
                warn!(%e, "stem worker: could not materialise stem_worker.py — deferring");
                return;
            }
        };

        match crate::stems::separator::separate_stems(
            &python,
            &script_path,
            &self.models_dir,
            &audio_path,
            &vocals_out,
            &instrumental_out,
            timeout,
            gpu_mem.as_deref(),
        )
        .await
        {
            Ok(()) => {
                match crate::db::models_stems::mark_stems_done(
                    &self.pool,
                    job.video_id,
                    &vocals_out.to_string_lossy(),
                    &instrumental_out.to_string_lossy(),
                )
                .await
                {
                    Ok(()) => info!(
                        video_id = job.video_id,
                        "stem worker: stems ready ({} + {})",
                        vocals_out.display(),
                        instrumental_out.display()
                    ),
                    Err(e) => {
                        error!(video_id = job.video_id, %e, "stem worker: mark_stems_done failed")
                    }
                }
            }
            Err(e) => {
                let prior: i64 =
                    sqlx::query_scalar("SELECT stem_attempts FROM videos WHERE id = ?")
                        .bind(job.video_id)
                        .fetch_one(&self.pool)
                        .await
                        .unwrap_or(0);
                let backoff = crate::downloader::retry_backoff(prior as u32 + 1);
                warn!(
                    video_id = job.video_id,
                    %e,
                    backoff_secs = backoff.as_secs(),
                    "stem worker: separation failed — deferring"
                );
                let _ = crate::db::models_stems::record_stem_deferral(
                    &self.pool,
                    job.video_id,
                    backoff,
                )
                .await;
            }
        }
    }

    /// Materialise `stem_worker.py` into `tools_dir`, mirroring the lyrics
    /// worker's `ensure_script`. The script is embedded at compile time via
    /// `include_str!`, so it always ships alongside the binary; it is (re)written
    /// only when the on-disk content differs, and the resolved path is returned.
    async fn ensure_script(&self) -> anyhow::Result<PathBuf> {
        const EMBEDDED: &str = include_str!("../../../../scripts/stem_worker.py");
        if let Some(parent) = self.script_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let stale = match tokio::fs::read_to_string(&self.script_path).await {
            Ok(existing) => existing != EMBEDDED,
            Err(_) => true,
        };
        if stale {
            tokio::fs::write(&self.script_path, EMBEDDED).await?;
            info!("stem_worker: wrote {}", self.script_path.display());
        }
        Ok(self.script_path.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_enabled_defaults_on_and_parses_off_values() {
        assert!(worker_enabled(None));
        assert!(worker_enabled(Some("true")));
        assert!(worker_enabled(Some("1")));
        assert!(!worker_enabled(Some("false")));
        assert!(!worker_enabled(Some("0")));
        assert!(!worker_enabled(Some(" OFF ")));
        assert!(!worker_enabled(Some("no")));
    }

    fn test_worker(pool: SqlitePool, tools_dir: PathBuf) -> StemWorker {
        StemWorker::new(
            pool,
            tools_dir,
            Arc::new(crate::playback::ndi_health::NdiHealthRegistry::new()),
            Arc::new(RwLock::new(crate::obs::ObsState::default())),
        )
    }

    #[tokio::test]
    async fn ensure_script_materialises_stem_worker_py() {
        let pool = crate::db::create_memory_pool().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let worker = test_worker(pool, dir.path().to_path_buf());

        let path = worker.ensure_script().await.unwrap();

        assert!(path.exists(), "stem_worker.py was not written");
        let written = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(written, include_str!("../../../../scripts/stem_worker.py"));
    }

    #[tokio::test]
    async fn missing_venv_python_warns_and_skips_without_touching_db() {
        let pool = crate::db::create_memory_pool().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        // No lyrics_venv under the tempdir → venv_python_path does not exist.
        let worker = test_worker(pool, dir.path().to_path_buf());

        worker.process_next().await;

        assert!(
            worker
                .warned_no_python
                .load(std::sync::atomic::Ordering::Relaxed),
            "tick should warn once when the venv python is missing"
        );
    }
}
