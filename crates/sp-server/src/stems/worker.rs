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
use crate::lyrics::idle_gate::{
    GateLog, WallActivity, should_defer, startup_floor_defers, wall_activity_from,
};
use crate::stems::worker_yield::{SepResult, Yield, run_separation_watched};

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
    /// A dub job needed the heavy slot and this separation yielded it (#184
    /// G0.1) — the child was killed, but the #171 resumable work dir + any
    /// partial stems are LEFT INTACT (the resume state), and the DB row is left
    /// pending (no backoff), so it resumes from its segments the next tick no dub
    /// is queued. Distinct from `WallAborted`, which DELETES the partial stems.
    YieldedToDub(String),
}

/// Videos longer than this are not separated; marked terminal (`unsupported`).
/// Raised from 15 min to 120 min (round G0, owner ruling 21.9.2026: every video,
/// incl. long dub videos, gets podklad/vokály stems — "na vsetko sa dava
/// rozdelenie"). The old 15-min rationale no longer holds: it assumed whole-file
/// separation that pinned the heavy child's memory near its Job Object ceiling
/// plus a single GPU-sized timeout. Separation now runs in resumable 30 s windows
/// (memory is per-segment, not per-file) and the timeout is duration-scaled (×4 on
/// a CPU plan) with heavy work at reduced priority during playback — so a 36-min
/// video is ~5-12 min of low-priority, resumable work. The 120-min ceiling is a
/// sanity bound (a multi-hour livestream stays excluded), not a "songs only" limit.
// Literal, not `120 * 60 * 1000` — cfg-independent arithmetic on a const is
// invisible to the mutation runner (same reasoning as heavy_slot.rs's ceiling).
pub(crate) const STEM_MAX_DURATION_MS: i64 = 7_200_000; // 120 min

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

/// #162 stem-worker per-tick defer decision. Pure w.r.t. `now`, so it is
/// unit-tested on BOTH mode arms (replacing the inline `mode == IdleOnly` branch,
/// whose `==` was a surviving mutant).
///
/// - [`LowPriority`](ProcessingMode::LowPriority) NEVER defers — it runs every
///   heavy separation at reduced priority instead.
/// - [`IdleOnly`](ProcessingMode::IdleOnly) defers on a busy wall using the
///   shared idle-settle hysteresis ([`GateLog::defer_settled`]) and emits the
///   once-per-transition INFO log via [`GateLog::note`].
///
/// Returns `Some(detail)` when this tick must defer (the caller returns early),
/// `None` to proceed to a job.
pub(crate) fn stem_defer_decision(
    mode: ProcessingMode,
    gate_enabled: bool,
    activity: WallActivity,
    gate_log: &mut GateLog,
    now: Instant,
) -> Option<&'static str> {
    match mode {
        ProcessingMode::LowPriority => None,
        ProcessingMode::IdleOnly => {
            if gate_log.defer_settled(gate_enabled, activity, now) {
                // When deferring only because the settle window has not elapsed
                // (the wall is idle right now), say so instead of "wall in use".
                let detail = if activity.in_use() {
                    activity.reason().unwrap_or("wall in use")
                } else {
                    "wall just went idle — settling"
                };
                if let Some(line) = gate_log.note(true, detail) {
                    info!("stem_worker: {line}");
                }
                Some(detail)
            } else {
                if let Some(line) = gate_log.note(false, "") {
                    info!("stem_worker: {line}");
                }
                None
            }
        }
    }
}

/// #162: the stem-separation subprocess timeout for `plan`. The base ceiling
/// (`isolation_timeout`) is sized for GPU speed; a CPU plan (cpu-idle, forced
/// onto CPU while the wall plays) is scaled by `CPU_TIMEOUT_MULTIPLIER` via
/// `heavy_step_timeout` so a CPU separation is not killed mid-run and retried
/// forever. Pure — unit-tested; each `separate_stems` spawn chooses its timeout
/// through this, from the plan it actually runs under.
pub(crate) fn separation_timeout(plan: &HeavyStepPlan, duration_ms: Option<i64>) -> Duration {
    crate::lyrics::heavy_plan::heavy_step_timeout(plan, isolation_timeout(duration_ms))
}

/// #162: the settle-free defer decision used when the gate-log mutex is
/// poisoned — only `idle-only` can defer, and only while the wall is in use.
/// Pure so both arms are unit-tested (`worker_plan_tests.rs`); the inline
/// expression let two mutants survive.
pub(crate) fn stem_defer_fallback(mode: ProcessingMode, activity: WallActivity) -> bool {
    mode == ProcessingMode::IdleOnly && should_defer(true, activity)
}

/// Whether stem separation is supported for a song of this duration. `None`
/// (duration unknown) is always supported — an unknown length must never
/// block separation, only a KNOWN duration past [`STEM_MAX_DURATION_MS`]
/// does. Pure — unit-tested directly with the exact boundary values.
pub(crate) fn stem_duration_supported(duration_ms: Option<i64>) -> bool {
    match duration_ms {
        None => true,
        Some(d) => d <= STEM_MAX_DURATION_MS,
    }
}

/// Positive-form twin of [`stem_duration_supported`] for the `process_next`
/// spawn seam: `true` when the song is too long and must be skipped. Avoids a
/// `!` at the call site (a deleted-`!` mutant there would be unobservable
/// without a real long-duration fixture); the decision itself is the
/// unit-tested `stem_duration_supported`.
pub(crate) fn stem_duration_too_long(duration_ms: Option<i64>) -> bool {
    !stem_duration_supported(duration_ms)
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

        // #184 G0.1: a dub job has priority on the heavy slot. While one is queued
        // behind it, skip this tick — start no new separation (the row stays
        // pending, no backoff, no DB write). The dub acquires within ~1 s, then
        // the flag clears and the stem queue resumes.
        if crate::stems::worker_yield::stem_tick_defers_to_dub(
            crate::lyrics::heavy_slot::dub_slot_wanted(),
        ) {
            info!("stem worker: a dub job is waiting for the heavy slot — deferring this tick");
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
        // #162: only `idle-only` mode can defer (idle-settle hysteresis +
        // once-per-transition log); `low-priority` NEVER defers. The mode branch
        // and the settle/log logic live in the pure `stem_defer_decision` so BOTH
        // arms are unit-tested. Held under one lock — no `.await` inside.
        let now = Instant::now();
        let defer = match self.wall_gate_log.lock() {
            Ok(mut g) => stem_defer_decision(mode, true, activity, &mut g, now).is_some(),
            // Poisoned lock: fall back to the settle-free decision (idle-only
            // only), no transition logging — unchanged from the pre-#162 path.
            Err(_) => stem_defer_fallback(mode, activity),
        };
        if defer {
            return;
        }

        // #195: serve the stems of what is actually IN USE first — the on-program
        // playlist (tier 1), then playlists played in the last `stems_recent_days`
        // days (tier 2), then today's unrestricted oldest-first query (tier 3);
        // manual priority (tier 0) still wins on any playlist. The tier inputs come
        // from the registry the worker already holds + one `play_history` query;
        // an empty list skips its tier.
        let (on_program, recent) = crate::stems::queue_tiers::compute_tier_inputs(
            self.ndi_health_registry.as_ref(),
            &self.pool,
        )
        .await;
        let job = match crate::db::models_stems_priority::get_next_stem_job(
            &self.pool,
            &on_program,
            &recent,
        )
        .await
        {
            Ok(Some(j)) => j,
            Ok(None) => return, // nothing to separate
            Err(e) => {
                warn!(%e, "stem worker: selection query failed");
                return;
            }
        };

        // Terminal skip: a video past the 120-min sanity ceiling (round G0 — a
        // multi-hour livestream would hold the heavy slot too long). Positive
        // form via `stem_duration_too_long` so no `!` sits at this seam; the
        // decision is the unit-tested `stem_duration_supported`. Done BEFORE the
        // startup floor: marking a row terminal-unsupported is a cheap DB
        // write, not a heavy step, so it must not be deferred by the startup
        // grace (it also skips the row for good, so deferring it just re-picks
        // the same doomed row every tick).
        if stem_duration_too_long(job.duration_ms) {
            // Raw milliseconds on purpose: a `/ 1000` here is log-only
            // arithmetic that no test can pin (surviving mutants).
            info!(
                video_id = job.video_id,
                duration_ms = job.duration_ms,
                "stem worker: skipping video_id={} ({} ms > {} ms) — stems only for videos up to 120 min",
                job.video_id,
                job.duration_ms.unwrap_or(0),
                STEM_MAX_DURATION_MS
            );
            let _ = crate::db::models_stems::mark_stems_unsupported(&self.pool, job.video_id).await;
            return;
        }

        // #167: no HEAVY step for the first 60 s after engine start — the wall
        // pipelines must come up on a fully quiet box (the post-deploy E2E samples
        // the engine in exactly this window). Gated here, AFTER the terminal skip
        // and before the actual separation; the row stays pending (no backoff),
        // re-picked next tick.
        if let Some(reg) = self.ndi_health_registry.as_ref()
            && startup_floor_defers(reg.since_created())
        {
            info!(
                video_id = job.video_id,
                "stem worker: heavy step separation deferred (wall unknown — startup grace)"
            );
            return;
        }

        let audio_path = PathBuf::from(&job.audio_file_path);
        let (vocals_out, instrumental_out) = crate::stems::stem_paths(&audio_path);
        // #171: resumable per-segment scratch dir beside the cached audio,
        // preserved across a stall/kill so the next pick resumes.
        let stem_work_dir = audio_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(format!("{}_stemsep", job.youtube_id));
        let gpu_mem = crate::db::models::get_setting(&self.pool, "lyrics_gpu_mem_fraction")
            .await
            .ok()
            .flatten();
        // #203: publish the live containment (CPU cap + affinity + memory
        // priority) so the Job Object seam applies it to this heavy child.
        crate::lyrics::heavy_slot::refresh_containment(&self.pool).await;
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

        // #144 r2: QUEUE for the heavy slot (fair FIFO — block behind a running
        // child), then measure headroom AT SPAWN with the permit held. Below the
        // 4 GiB floor → release the permit and leave the row PENDING with NO
        // `record_stem_deferral` (no backoff); re-picked next tick. The WARN
        // fires inside `memory_ok_for`. Held across `run_separation_watched`
        // (incl. the GPU→CPU re-run below), so the deep acquire in
        // `separator::separate_stems` is gone — a second acquire on the same task
        // would deadlock the Semaphore(1). A queued dub still preempts via the
        // watcher: the guard drops when this tick ends and the dub (next in FIFO)
        // proceeds.
        let _slot = match crate::lyrics::heavy_slot::acquire_slot_for_spawn("stem separation").await
        {
            Ok(g) => g,
            Err(_) => return,
        };

        // #177: publish the live in-flight signal for the karaoke panel's ⚙
        // "spracúvam" state; the guard clears it when this scope ends (success,
        // failure, wall-abort, or dub-yield).
        let _in_flight = crate::stems::progress::begin(job.video_id);
        let plan = HeavyStepPlan::for_activity(mode, activity);
        // #162: the timeout for THIS plan — a cpu-idle plan gets the ×4 base so a
        // slow CPU separation is not killed mid-run and retried forever.
        let timeout = separation_timeout(&plan, job.duration_ms);
        info!(
            video_id = job.video_id,
            "stem worker: heavy step separation mode={} timeout={}s (wall {})",
            plan.label(),
            timeout.as_secs(),
            if activity.in_use() {
                activity.reason().unwrap_or("wall in use")
            } else {
                "idle"
            }
        );
        // #184 G0.1: run the separation under the dub-yield watcher for EVERY plan
        // — a queued dub preempts ANY plan (within ~1 s), and a busy wall still
        // preempts a GPU plan (#161, 2-consecutive-busy debounce; a cpu-idle plan
        // cannot disturb the wall so it is never wall-yielded). `separate_stems`
        // is remote-free.
        let step = match run_separation_watched(
            crate::stems::separator::separate_stems(
                &python,
                &script_path,
                &self.models_dir,
                &audio_path,
                &vocals_out,
                &instrumental_out,
                &stem_work_dir,
                timeout,
                gpu_mem.as_deref(),
                &plan,
            ),
            plan,
            || wall_activity_from(self.ndi_health_registry.as_ref(), self.obs_state.as_ref()),
        )
        .await
        {
            SepResult::Done => StemStepResult::Done,
            SepResult::Failed(e) => StemStepResult::Failed(e),
            // A dub needed the slot — leave the row + resume state intact (#171).
            SepResult::Yielded {
                reason: Yield::Dub,
                detail,
            } => StemStepResult::YieldedToDub(detail),
            // A GPU separation aborted because the wall went busy (only a GPU plan
            // yields Wall). Low-priority re-runs on CPU (also watched, so a dub can
            // still preempt); idle-only re-queues with NO penalty (#161).
            SepResult::Yielded {
                reason: Yield::Wall,
                detail,
            } => match mode {
                ProcessingMode::LowPriority => {
                    let cpu_plan = HeavyStepPlan::cpu_idle();
                    let cpu_timeout = separation_timeout(&cpu_plan, job.duration_ms);
                    info!(
                        video_id = job.video_id,
                        "stem worker: heavy step separation re-run mode=cpu-idle \
                         timeout={}s after GPU abort ({})",
                        cpu_timeout.as_secs(),
                        detail
                    );
                    match run_separation_watched(
                        crate::stems::separator::separate_stems(
                            &python,
                            &script_path,
                            &self.models_dir,
                            &audio_path,
                            &vocals_out,
                            &instrumental_out,
                            &stem_work_dir,
                            cpu_timeout,
                            gpu_mem.as_deref(),
                            &cpu_plan,
                        ),
                        cpu_plan,
                        || {
                            wall_activity_from(
                                self.ndi_health_registry.as_ref(),
                                self.obs_state.as_ref(),
                            )
                        },
                    )
                    .await
                    {
                        SepResult::Done => StemStepResult::Done,
                        SepResult::Failed(e) => StemStepResult::Failed(e),
                        // A cpu-idle re-run can only yield to a dub (`yield_reason`
                        // never returns Wall for a CPU plan); resume no-penalty.
                        SepResult::Yielded { detail, .. } => StemStepResult::YieldedToDub(detail),
                    }
                }
                ProcessingMode::IdleOnly => StemStepResult::WallAborted(detail),
            },
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
            StemStepResult::YieldedToDub(detail) => {
                // #184 G0.1: leave EVERYTHING as-is — the #171 resumable segments
                // in the work dir are the resume state, and the DB row stays
                // pending (stem_status NULL, stem_attempts unchanged, no
                // stem_next_attempt_at), so the selector re-picks it the moment no
                // dub is queued. Distinct from WallAborted, which DELETES the
                // partial stems.
                info!(
                    video_id = job.video_id,
                    detail = %detail,
                    "stem worker: yielded the heavy slot to a dub job"
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
#[path = "worker_plan_tests.rs"]
mod plan_tests;

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

    /// #184 G0.1: a dub-yield leaves the RESUME STATE intact — partial stems +
    /// the #171 work dir are KEPT, and the DB row stays pending (stem_status
    /// NULL, stem_attempts unchanged, no stem_next_attempt_at) so the selector
    /// re-picks it. Distinct from WallAborted, which DELETES the partial stems.
    #[tokio::test]
    async fn yield_to_dub_keeps_resume_state_and_leaves_row_pending() {
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        seed_pending_stem_row(&pool, 1).await;
        let dir = tempfile::tempdir().unwrap();
        // Partial stem outputs + a resumable work dir the yield must PRESERVE.
        let vocals = dir.path().join("v.flac");
        let instr = dir.path().join("i.flac");
        std::fs::write(&vocals, b"partial").unwrap();
        std::fs::write(&instr, b"partial").unwrap();
        let work_dir = dir.path().join("yt_stem_stemsep");
        std::fs::create_dir_all(&work_dir).unwrap();
        std::fs::write(work_dir.join("seg_000.wav"), b"seg").unwrap();
        let worker = test_worker(pool.clone(), dir.path().to_path_buf());

        worker
            .record_stem_result(
                &stem_job(1),
                &vocals,
                &instr,
                StemStepResult::YieldedToDub("dub job waiting for the heavy slot".into()),
            )
            .await;

        let (status, attempts, next): (Option<String>, i64, Option<String>) = sqlx::query_as(
            "SELECT stem_status, stem_attempts, stem_next_attempt_at FROM videos WHERE id = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(status.is_none(), "a dub-yield must leave stem_status NULL");
        assert_eq!(attempts, 0, "a dub-yield must not increment stem_attempts");
        assert!(
            next.is_none(),
            "a dub-yield must set no stem_next_attempt_at (no backoff)"
        );
        assert!(
            vocals.exists(),
            "the partial vocals stem must be KEPT — the #171 resume state"
        );
        assert!(instr.exists(), "the partial instrumental stem must be KEPT");
        assert!(
            work_dir.join("seg_000.wav").exists(),
            "the resumable work dir must be KEPT"
        );

        let next_job = crate::db::models_stems::get_next_video_for_stems(&pool)
            .await
            .unwrap();
        assert_eq!(
            next_job.map(|j| j.video_id),
            Some(1),
            "the yielded row must be re-picked (still pending)"
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

    // ---- duration terminal-skip (2026-09-15) ------------------------------

    /// A worker whose venv python "exists" (an empty stub file at the
    /// platform-correct path), so `process_next` clears the venv gate and
    /// reaches the terminal duration-skip check.
    fn worker_with_stub_venv(pool: SqlitePool, dir: &std::path::Path) -> StemWorker {
        let python = crate::lyrics::bootstrap::venv_python_path(dir);
        if let Some(parent) = python.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&python, b"").unwrap();
        test_worker(pool, dir.to_path_buf())
    }

    /// A 121-minute row (over the 120-min `STEM_MAX_DURATION_MS` cap) must be
    /// marked terminal `unsupported`, with no backoff bookkeeping touched —
    /// mirrors the exact-boundary coverage in `stem_duration_supported`'s pure
    /// tests (`worker_plan_tests.rs`), but proves the worker's real spawn seam
    /// (`stem_duration_too_long`) actually gates on it.
    #[tokio::test]
    async fn process_next_marks_overlong_song_unsupported() {
        // This test reaches the #184 G0.1 dub tick-defer (stub venv passes the
        // venv gate), so serialize + clear the process-global dub flag so a
        // parallel flag test can't make it defer instead of marking the row.
        let _lk = crate::lyrics::heavy_slot::DUB_FLAG_SERIAL.lock().await;
        crate::lyrics::heavy_slot::set_dub_slot_wanted(false);
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        seed_pending_stem_row(&pool, 1).await;
        sqlx::query("UPDATE videos SET duration_ms = ? WHERE id = 1")
            .bind(7_260_000i64) // 121 min
            .execute(&pool)
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let worker = worker_with_stub_venv(pool.clone(), dir.path());

        worker.process_next().await;

        let (status, attempts): (Option<String>, i64) =
            sqlx::query_as("SELECT stem_status, stem_attempts FROM videos WHERE id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            status.as_deref(),
            Some("unsupported"),
            "a 121-min song must be marked terminal unsupported"
        );
        assert_eq!(
            attempts, 0,
            "the duration skip must not touch stem_attempts — it is not a backoff"
        );
    }

    /// A normal-length (4-min) row is never touched by the terminal-skip path.
    /// No stub venv is provided here — `process_next` stops at the missing
    /// venv-python gate before reaching the duration check at all (same as
    /// `missing_venv_python_warns_and_skips_without_touching_db`); that is
    /// fine, since the exact 120-min boundary is already proven by the pure
    /// `stem_duration_supported` tests. This just proves a normal row is left
    /// pending, never spuriously marked unsupported.
    #[tokio::test]
    async fn process_next_leaves_a_normal_length_song_pending() {
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        seed_pending_stem_row(&pool, 1).await;
        sqlx::query("UPDATE videos SET duration_ms = ? WHERE id = 1")
            .bind(240_000i64) // 4 min
            .execute(&pool)
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let worker = test_worker(pool.clone(), dir.path().to_path_buf());

        worker.process_next().await;

        let status: Option<String> =
            sqlx::query_scalar("SELECT stem_status FROM videos WHERE id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(status.is_none(), "a 4-minute song must stay pending");
    }
}
