//! Background karaoke stem-separation worker (#14).
//!
//! Mirrors the lyrics worker: a periodic tick that, ONLY while the wall is idle
//! (#154 gate — reused verbatim), separates the next normalized song into its
//! vocals + instrumental sidecars via `scripts/stem_worker.py`. Runs at lowest
//! priority (after lyrics), backoff-gated so a broken row never hot-loops.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sqlx::SqlitePool;
use tokio::sync::{RwLock, broadcast};
use tracing::{error, info, warn};

use crate::lyrics::aligner::isolation_timeout;
use crate::lyrics::heavy_plan::{HeavyStepPlan, ProcessingMode};
use crate::lyrics::idle_gate::{GateLog, should_defer, wall_activity_from};
use crate::lyrics::idle_gate_abort::run_with_wall_abort;

/// #161: outcome of one stem-separation attempt run under the wall-abort
/// watcher. Distinguishes a genuine failure (backoff-deferred) from a wall
/// abort (re-queued with no penalty) at the `separate_stems` return boundary.
enum StemStepResult {
    /// Separation completed — record the two stems.
    Done,
    /// A genuine separation failure — record the backoff deferral.
    Failed(anyhow::Error),
    /// The wall became busy mid-run and the separator was killed — delete any
    /// partial stems and leave the DB row pending (no penalty).
    WallAborted(String),
}

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

        // #162 priority regime (same `lyrics_processing_mode` switch as the
        // lyrics worker). `idle-only` defers heavy separation while the wall is
        // in use (with idle-settle hysteresis); `low-priority` (default) NEVER
        // defers — it runs the separator at reduced priority instead (CPU-idle
        // while the wall plays), so the stem queue drains continuously.
        let mode = ProcessingMode::from_setting(
            crate::db::models::get_setting(&self.pool, "lyrics_processing_mode")
                .await
                .ok()
                .flatten()
                .as_deref(),
        );
        let activity =
            wall_activity_from(self.ndi_health_registry.as_ref(), self.obs_state.as_ref()).await;
        if mode == ProcessingMode::IdleOnly {
            // Idle-settle hysteresis (2026-09-14 incident): a single idle sample
            // is not enough — resume only after the wall has read idle
            // continuously for `WALL_IDLE_SETTLE`. Shared with the lyrics worker
            // via `GateLog`. Gate is definitionally ON in idle-only mode.
            let now = Instant::now();
            let defer = match self.wall_gate_log.lock() {
                Ok(mut g) => g.defer_settled(true, activity, now),
                Err(_) => should_defer(true, activity),
            };
            if defer {
                // When deferring only because the settle window has not elapsed
                // (the wall is idle right now), say so instead of "wall in use".
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

        // #162: separate under the priority regime. In `low-priority` while the
        // wall is in use the plan is CPU-idle — it is NEVER aborted (a CPU/IDLE
        // job cannot disturb the wall). A GPU plan (wall idle, or idle-only) runs
        // under the mid-job abort watcher: on a busy wall the child is killed
        // (`kill_on_drop`); in `low-priority` we re-run IMMEDIATELY on CPU, in
        // `idle-only` we re-queue with NO penalty (`StemStepResult::WallAborted`
        // → `stem_status` stays NULL). `separate_stems` is remote-free.
        let plan = HeavyStepPlan::for_activity(mode, activity);
        info!(
            video_id = job.video_id,
            "stem worker: heavy step separation mode={} (wall {})",
            plan.label(),
            if activity.in_use() {
                activity.reason().unwrap_or("wall in use")
            } else {
                "idle"
            }
        );
        let step = if !plan.is_gpu() {
            match crate::stems::separator::separate_stems(
                &python,
                &script_path,
                &self.models_dir,
                &audio_path,
                &vocals_out,
                &instrumental_out,
                timeout,
                gpu_mem.as_deref(),
                &plan,
            )
            .await
            {
                Ok(()) => StemStepResult::Done,
                Err(e) => StemStepResult::Failed(e),
            }
        } else {
            match run_with_wall_abort(
                crate::stems::separator::separate_stems(
                    &python,
                    &script_path,
                    &self.models_dir,
                    &audio_path,
                    &vocals_out,
                    &instrumental_out,
                    timeout,
                    gpu_mem.as_deref(),
                    &plan,
                ),
                true,
                || wall_activity_from(self.ndi_health_registry.as_ref(), self.obs_state.as_ref()),
            )
            .await
            {
                Ok(Ok(())) => StemStepResult::Done,
                Ok(Err(e)) => StemStepResult::Failed(e),
                Err(abort) => match mode {
                    ProcessingMode::LowPriority => {
                        info!(
                            video_id = job.video_id,
                            "stem worker: heavy step separation re-run mode=cpu-idle \
                             after GPU abort ({})",
                            abort.detail
                        );
                        match crate::stems::separator::separate_stems(
                            &python,
                            &script_path,
                            &self.models_dir,
                            &audio_path,
                            &vocals_out,
                            &instrumental_out,
                            timeout,
                            gpu_mem.as_deref(),
                            &HeavyStepPlan::cpu_idle(),
                        )
                        .await
                        {
                            Ok(()) => StemStepResult::Done,
                            Err(e) => StemStepResult::Failed(e),
                        }
                    }
                    ProcessingMode::IdleOnly => StemStepResult::WallAborted(abort.detail),
                },
            }
        };
        self.record_stem_result(&job, &vocals_out, &instrumental_out, step)
            .await;
    }

    /// #161: apply the outcome of one separation attempt. `Done` records the
    /// stems; `Failed` records the backoff deferral (the unchanged pre-#161
    /// behaviour); `WallAborted` deletes any partial stems and writes NOTHING to
    /// the DB, so `stem_status` stays NULL and `stem_attempts` is unchanged —
    /// `get_next_video_for_stems` re-picks the row the instant the wall idles.
    async fn record_stem_result(
        &self,
        job: &crate::db::models_stems::StemJob,
        vocals_out: &Path,
        instrumental_out: &Path,
        result: StemStepResult,
    ) {
        match result {
            StemStepResult::Done => {
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
            StemStepResult::Failed(e) => {
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
            StemStepResult::WallAborted(detail) => {
                // Delete the aborted step's partial stem outputs; do NOT touch
                // the DB — no backoff, no 'failed' status. The row stays pending
                // (stem_status NULL, stem_attempts unchanged) and is re-picked
                // the moment the wall goes idle again.
                let _ = tokio::fs::remove_file(vocals_out).await;
                let _ = tokio::fs::remove_file(instrumental_out).await;
                info!(
                    video_id = job.video_id,
                    detail = %detail,
                    "stem worker: aborted heavy step — wall became busy — re-queued no-penalty (#161)"
                );
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

    // ---- #161 mid-job wall-abort re-queue semantics -----------------------

    /// Seed playlist 1 (FK target) + one pending stem row (normalized, has an
    /// audio path, stem_status NULL). Mirrors `worker_tests_idle_gate.rs`.
    async fn seed_pending_stem_row(pool: &SqlitePool, video_id: i64) {
        sqlx::query(
            "INSERT OR IGNORE INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
             VALUES (1, 'stem_pl', 'u', 'SP-fast', 1)",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, normalized, audio_file_path) \
             VALUES (?, 1, 'yt_stem', 1, '/tmp/x_audio.flac')",
        )
        .bind(video_id)
        .execute(pool)
        .await
        .unwrap();
    }

    fn stem_job(video_id: i64) -> crate::db::models_stems::StemJob {
        crate::db::models_stems::StemJob {
            video_id,
            youtube_id: "yt_stem".into(),
            audio_file_path: "/tmp/x_audio.flac".into(),
            duration_ms: Some(180_000),
            song: Some("s".into()),
            artist: Some("a".into()),
        }
    }

    /// A wall abort re-queues with NO penalty: partial stems deleted, DB row
    /// left pending (stem_status NULL, stem_attempts unchanged), and the
    /// selector re-picks it immediately.
    #[tokio::test]
    async fn wall_abort_re_queues_with_no_penalty() {
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        seed_pending_stem_row(&pool, 1).await;
        let dir = tempfile::tempdir().unwrap();
        // Partial stem outputs the abort must delete.
        let vocals = dir.path().join("v.flac");
        let instr = dir.path().join("i.flac");
        std::fs::write(&vocals, b"partial").unwrap();
        std::fs::write(&instr, b"partial").unwrap();
        let worker = test_worker(pool.clone(), dir.path().to_path_buf());

        worker
            .record_stem_result(
                &stem_job(1),
                &vocals,
                &instr,
                StemStepResult::WallAborted("output playing".into()),
            )
            .await;

        let (status, attempts): (Option<String>, i64) =
            sqlx::query_as("SELECT stem_status, stem_attempts FROM videos WHERE id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(status.is_none(), "wall-abort must leave stem_status NULL");
        assert_eq!(attempts, 0, "wall-abort must not increment stem_attempts");
        assert!(!vocals.exists(), "partial vocals stem must be deleted");
        assert!(!instr.exists(), "partial instrumental stem must be deleted");

        let next = crate::db::models_stems::get_next_video_for_stems(&pool)
            .await
            .unwrap();
        assert_eq!(
            next.map(|j| j.video_id),
            Some(1),
            "the aborted row must be re-picked immediately"
        );
    }

    /// A GENUINE separation failure still records the backoff deferral —
    /// distinct from a wall abort.
    #[tokio::test]
    async fn genuine_failure_still_records_the_deferral() {
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        seed_pending_stem_row(&pool, 1).await;
        let dir = tempfile::tempdir().unwrap();
        let worker = test_worker(pool.clone(), dir.path().to_path_buf());

        worker
            .record_stem_result(
                &stem_job(1),
                &dir.path().join("v.flac"),
                &dir.path().join("i.flac"),
                StemStepResult::Failed(anyhow::anyhow!("separator boom")),
            )
            .await;

        let (status, attempts): (Option<String>, i64) =
            sqlx::query_as("SELECT stem_status, stem_attempts FROM videos WHERE id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            status.as_deref(),
            Some("failed"),
            "a real failure records 'failed'"
        );
        assert_eq!(
            attempts, 1,
            "a real failure increments stem_attempts (backoff)"
        );
    }
}
