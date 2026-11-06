//! Background karaoke stem-separation worker (#14).
//!
//! Mirrors the lyrics worker: a periodic tick that separates the next normalized
//! song into its vocals + instrumental sidecars via `scripts/stem_worker.py`,
//! at reduced priority (#162: CPU-idle while the wall is in use, GPU while it
//! is idle), backoff-gated so a broken row never hot-loops.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;
use tokio::sync::{RwLock, broadcast};
use tracing::{error, info, warn};

use crate::lyrics::aligner::isolation_timeout;
use crate::lyrics::heavy_plan::HeavyStepPlan;
use crate::lyrics::idle_gate::{startup_floor_defers, wall_activity_from};
use crate::stems::worker_yield::{SepResult, Yield, run_separation_watched};

/// #161: outcome of one stem-separation attempt run under the wall-abort
/// watcher. Distinguishes a genuine failure (backoff-deferred) from a dub
/// yield (resumed with no penalty) at the `separate_stems` return boundary.
enum StemStepResult {
    /// Separation completed — record the two stems.
    Done,
    /// A genuine separation failure — record the backoff deferral.
    Failed(anyhow::Error),
    /// A dub job needed the heavy slot and this separation yielded it (#184
    /// G0.1) — the child was killed, but the #171 resumable work dir + any
    /// partial stems are LEFT INTACT (the resume state), and the DB row is left
    /// pending (no backoff), so it resumes from its segments the next tick no dub
    /// is queued.
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
    /// Logged once when python is unavailable so the operator sees why nothing
    /// is separating, without spamming every tick.
    warned_no_python: std::sync::atomic::AtomicBool,
    /// #229: this node in the exchange, asked before each separation (`None`
    /// in tests that do not need it).
    peer: Option<Arc<crate::peer::Exchange>>,
}

/// Parse the `stem_worker_enabled` setting. Default ON so a fresh deploy starts
/// separating; `false`/`0`/`off`/`no` disable it. Mirrors
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

/// #162: the stem-separation subprocess timeout for `plan`. The base ceiling
/// (`isolation_timeout`) is sized for GPU speed; a CPU plan (cpu-idle, forced
/// onto CPU while the wall plays) is scaled by `CPU_TIMEOUT_MULTIPLIER` via
/// `heavy_step_timeout` so a CPU separation is not killed mid-run and retried
/// forever. Pure — unit-tested; each `separate_stems` spawn chooses its timeout
/// through this, from the plan it actually runs under.
pub(crate) fn separation_timeout(plan: &HeavyStepPlan, duration_ms: Option<i64>) -> Duration {
    crate::lyrics::heavy_plan::heavy_step_timeout(plan, isolation_timeout(duration_ms))
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
            warned_no_python: std::sync::atomic::AtomicBool::new(false),
            peer: None,
        }
    }

    /// #229: ask the exchange's peers before each separation.
    pub fn with_peer(mut self, peer: Arc<crate::peer::Exchange>) -> Self {
        self.peer = Some(peer);
        self
    }

    /// The missing-venv WARN, once per process (`warned_no_python` is its
    /// once-guard). Logging only.
    #[cfg_attr(test, mutants::skip)]
    fn warn_no_venv_once(&self, python: &Path) {
        if !self
            .warned_no_python
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            warn!(
                "stem worker: lyrics venv python not found at {python:?} — this node separates no stems until the lyrics bootstrap"
            );
        }
    }

    /// #229: with no venv (WARNed once), whether this tick goes on: only on a
    /// node that asks its peers (it still takes a peer's stems); a node that
    /// asks none (SNV in phase 1) or has no exchange stops at the venv as
    /// before.
    async fn no_venv_asks_peers(&self, python: &Path) -> bool {
        self.warn_no_venv_once(python);
        match &self.peer {
            Some(ex) => ex.asks_peers().await,
            None => false,
        }
    }

    /// #229: with no venv this node separates nothing, so a row it would run
    /// here is put back for `INPUT_MISSING_RECHECK` (no attempt, status
    /// untouched): the rows behind it reach their peer step too.
    async fn no_venv_puts_back(&self, video_id: i64) {
        let wait = crate::song_input::INPUT_MISSING_RECHECK;
        if let Err(e) = crate::db::models_stems::defer_stems(&self.pool, video_id, wait).await {
            warn!(video_id, %e, "stem worker: putting a row back (no venv) failed");
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
        // #230: a held background starts no new job (a running one finishes).
        if crate::background_hold::holds(&self.pool, crate::background_hold::Job::Stems).await {
            return;
        }

        // #229: a node with no lyrics venv that asks its peers still takes a
        // peer's stems (the plan's decisions: the fetch runs before the venv
        // check); its venv return waits until after the peer step below.
        let python = crate::lyrics::bootstrap::venv_python_path(&self.tools_dir);
        let venv_missing = !python.exists();
        if venv_missing && !self.no_venv_asks_peers(&python).await {
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

        // #162 priority regime: the separator never waits for an idle wall —
        // it runs at reduced priority (CPU-idle while the wall plays), so the
        // stem queue drains continuously.
        let activity =
            wall_activity_from(self.ndi_health_registry.as_ref(), self.obs_state.as_ref()).await;
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

        // #229: ask the peers first (`peer::stems`): a peer's stems are taken,
        // a peer's separation waited for; else separate here, announced until
        // this tick ends. Before the venv check: a node with no venv still
        // takes a peer's stems.
        let _announced = match crate::peer::stems::first(self.peer.as_ref(), &job).await {
            crate::peer::PeerStep::Local(guard) => guard,
            crate::peer::PeerStep::Done | crate::peer::PeerStep::Deferred => return,
        };
        if venv_missing {
            return self.no_venv_puts_back(job.video_id).await;
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
        // #136: separate the audio the row records NOW — a rename (the metadata
        // repair) while this job waited for the slot moved it. No audio on disk
        // = a no-penalty re-pick later (`song_input::job_input`).
        let Some(input) = crate::song_input::job_input(
            &self.pool,
            job.video_id,
            &job.audio_file_path,
            crate::song_input::HeavyJob::Stems,
        )
        .await
        else {
            return;
        };
        let job = input.stem_job(job);
        let audio_path = PathBuf::from(&job.audio_file_path);
        let (vocals_out, instrumental_out) = crate::stems::stem_paths(&audio_path);
        // #171: resumable per-segment scratch dir beside the cached audio,
        // preserved across a stall/kill so the next pick resumes.
        let stem_work_dir = audio_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(format!("{}_stemsep", job.youtube_id));

        // #177: publish the live in-flight signal for the karaoke panel's ⚙
        // "spracúvam" state; the guard clears it when this scope ends (success,
        // failure, wall-abort, or dub-yield).
        let _in_flight = crate::stems::progress::begin(job.video_id);
        let plan = HeavyStepPlan::for_activity(activity);
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
            // yields Wall): re-run on CPU (also watched, so a dub can still
            // preempt).
            SepResult::Yielded {
                reason: Yield::Wall,
                detail,
            } => {
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
        };
        self.record_stem_result(&job, &vocals_out, &instrumental_out, step)
            .await;
    }

    /// #161: apply the outcome of one separation attempt. `Done` records the
    /// stems; `Failed` records the backoff deferral (the unchanged pre-#161
    /// behaviour); `YieldedToDub` leaves the row and its resume state as they
    /// are.
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
                // #136: the stems carry the name the song had when the job
                // started; a rename while it ran (the metadata repair) left them
                // behind. Bring them under the audio's CURRENT name.
                let cache_dir = Path::new(&job.audio_file_path)
                    .parent()
                    .unwrap_or_else(|| Path::new("."));
                if let Err(e) = crate::song_relink::relink_song(
                    &self.pool,
                    cache_dir,
                    job.video_id,
                    Path::new(&job.audio_file_path),
                )
                .await
                {
                    warn!(video_id = job.video_id, %e, "stem worker: stems re-link failed");
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
            StemStepResult::YieldedToDub(detail) => {
                // #184 G0.1: leave EVERYTHING as-is — the #171 resumable segments
                // in the work dir are the resume state, and the DB row stays
                // pending (stem_status NULL, stem_attempts unchanged, no
                // stem_next_attempt_at), so the selector re-picks it the moment no
                // dub is queued.
                info!(
                    video_id = job.video_id,
                    detail = %detail,
                    "stem worker: yielded the heavy slot to a dub job"
                );
            }
        }
    }

    /// Materialise the stem tool scripts into `tools_dir` (`stem_worker.py` +
    /// the `win_replace.py` it imports, #207 — `super::scripts`), rewriting
    /// only the stale ones, and return the worker script path.
    async fn ensure_script(&self) -> anyhow::Result<PathBuf> {
        let scripts = super::scripts::embedded_tool_scripts();
        crate::embedded_scripts::materialise(&self.tools_dir, &scripts, "stem_worker").await?;
        Ok(self.script_path.clone())
    }
}

#[cfg(test)]
#[path = "worker_plan_tests.rs"]
mod plan_tests;

#[cfg(test)]
#[path = "worker_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "worker_tests_peer.rs"]
mod tests_peer;
