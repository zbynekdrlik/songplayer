//! #161 — mid-job wall-abort: kill a RUNNING heavy GPU step when the wall
//! becomes busy, so a job started while the wall was idle does not keep
//! starving the live LED-wall decoder/NDI path for its 2–5 min GPU run.
//!
//! Companion to `idle_gate.rs` (the #154 PRE-flight gate): that decides whether
//! to START a heavy step; this watches one already RUNNING. The pure
//! [`AbortPolicy`] (consecutive-busy debounce) is unit-tested directly;
//! [`run_with_wall_abort`] races the heavy future against a 1 s wall poll and
//! DROPS it on abort — every heavy child (`preprocess_vocals`, `mtl_aligner`,
//! `separate_stems`) sets `kill_on_drop(true)`, so dropping the future SIGKILLs
//! the subprocess. Split into its own module so `worker.rs` / `stems/worker.rs`
//! stay under the 1000-line cap.
//!
//! It only changes WHEN a heavy stage runs, never its output (the aborted step
//! re-runs to identical bytes on the next idle pick), so it is NOT a
//! `LYRICS_PIPELINE_VERSION` bump.

use std::path::PathBuf;
use std::time::Duration;

use super::idle_gate::WallActivity;

/// Consecutive ~1 s busy samples before a running heavy step is aborted. Two
/// samples ≈ 2 s debounce: a transient scene switch / song change that flips a
/// pipeline off (or momentarily onto) Playing for a beat must not kill a
/// legitimate in-flight job. Deliberately NOT the 30 s `WALL_IDLE_SETTLE` — that
/// guards RESUME (don't restart during a flicker); this guards RUN and must
/// react in ~2 s or the wall keeps stuttering.
pub(crate) const ABORT_CONSECUTIVE_BUSY: u32 = 2;

/// How often the abort watcher samples the wall while a heavy step runs.
pub(crate) const ABORT_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Pure abort decision: count consecutive busy samples, abort once the run
/// reaches [`ABORT_CONSECUTIVE_BUSY`]. A disabled gate or an idle sample resets
/// the count and never aborts. No I/O — [`run_with_wall_abort`] feeds it live
/// samples.
#[derive(Debug, Default)]
pub(crate) struct AbortPolicy {
    consecutive_busy: u32,
}

impl AbortPolicy {
    /// Feed one wall sample. Returns `true` when the running heavy step must be
    /// aborted NOW: the gate is enabled AND the wall has read busy on
    /// [`ABORT_CONSECUTIVE_BUSY`] consecutive samples. Gate off OR an idle
    /// sample resets the streak and returns `false`.
    pub(crate) fn observe(&mut self, gate_enabled: bool, in_use: bool) -> bool {
        if !gate_enabled || !in_use {
            self.consecutive_busy = 0;
            return false;
        }
        self.consecutive_busy += 1;
        self.consecutive_busy >= ABORT_CONSECUTIVE_BUSY
    }
}

/// The error [`run_with_wall_abort`] returns when it kills a running heavy step
/// because the wall became busy. `detail` names the cause (e.g. `"output
/// playing"`) for the INFO log / dashboard badge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WallAbort {
    pub(crate) detail: String,
}

impl std::fmt::Display for WallAbort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "aborted heavy step — wall became busy ({})", self.detail)
    }
}

impl std::error::Error for WallAbort {}

/// Race `fut` (a heavy GPU-subprocess future whose child sets `kill_on_drop`)
/// against a 1 s wall-activity poll. Returns `Ok(fut's output)` if it finishes
/// first; returns `Err(WallAbort)` — DROPPING `fut`, which SIGKILLs the child —
/// once [`AbortPolicy`] sees [`ABORT_CONSECUTIVE_BUSY`] consecutive busy
/// samples. `gate_enabled = false` never aborts (the future always runs to
/// completion). `wall` re-reads the live wall snapshot each tick (e.g.
/// `idle_gate::wall_activity_from`).
#[cfg_attr(test, mutants::skip)] // timing orchestration; the decision core (AbortPolicy) is unit-tested and a live subprocess is integration-only.
pub(crate) async fn run_with_wall_abort<T, W, Fut>(
    fut: impl std::future::Future<Output = T>,
    gate_enabled: bool,
    mut wall: W,
) -> Result<T, WallAbort>
where
    W: FnMut() -> Fut,
    Fut: std::future::Future<Output = WallActivity>,
{
    // Gate off → the watcher can never abort, so skip the 1 s ticker + wall
    // reads entirely and just run the step to completion.
    if !gate_enabled {
        return Ok(fut.await);
    }
    tokio::pin!(fut);
    let mut policy = AbortPolicy::default();
    let mut ticker = tokio::time::interval(ABORT_POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // `interval`'s first tick fires immediately; consume it so the first real
    // sample is one interval into the run, not at t=0.
    ticker.tick().await;
    loop {
        tokio::select! {
            out = &mut fut => return Ok(out),
            _ = ticker.tick() => {
                let activity = wall().await;
                if policy.observe(gate_enabled, activity.in_use()) {
                    let detail = activity.reason().unwrap_or("wall in use").to_string();
                    return Err(WallAbort { detail });
                }
            }
        }
    }
}

/// #162: the vocal-isolation subprocess timeout for `plan`. The base ceiling
/// [`aligner::isolation_timeout`] is sized for GPU speed; a CPU plan (cpu-idle,
/// forced onto CPU while the wall plays) is scaled by `CPU_TIMEOUT_MULTIPLIER`
/// via [`heavy_step_timeout`] so a CPU isolation is not killed mid-run. Pure —
/// unit-tested; `isolate_vocals` chooses its timeout through this.
pub(crate) fn isolation_step_timeout(
    plan: &crate::lyrics::heavy_plan::HeavyStepPlan,
    duration_ms: Option<i64>,
) -> Duration {
    crate::lyrics::heavy_plan::heavy_step_timeout(
        plan,
        crate::lyrics::aligner::isolation_timeout(duration_ms),
    )
}

/// #144: what the ★ isolation step should do for a song, decided from the
/// stems worker's state instead of running a second (BS-RoFormer) isolation
/// pass. Every video already gets a vocals sidecar from the stems worker
/// (#184 G0, `{base}_audio_vocals.flac`), so the mtl aligner's vocals come from
/// THAT — `preprocess-vocals` is now only anvuew dereverb + 16 kHz resample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IsolationInput {
    /// The stems worker finished and the vocals sidecar exists on disk — feed
    /// this path into the dereverb + resample step.
    Stems(PathBuf),
    /// The stems worker has not produced the vocals sidecar yet — defer the
    /// song to the queue with NO penalty (no heavy child spawned); the stems
    /// worker drains the whole catalogue, so it will get there.
    WaitForStems,
    /// The stems worker will NEVER produce vocals for this song (terminal
    /// `'unsupported'` — too long / no vocals). No isolation path exists any
    /// more, so the song takes the g35t base tier without spawning a child.
    BaseTierOnly,
}

/// Pure decision for [`IsolationInput`] from the row's raw `stem_status`
/// (`crate::db::models_stems` vocabulary: NULL = pending, `'done'`, `'failed'`,
/// `'unsupported'`) and whether the vocals sidecar is on disk.
///
/// - `'unsupported'` → [`IsolationInput::BaseTierOnly`] (terminal; no stems ever).
/// - `'done'` AND the vocals file exists → [`IsolationInput::Stems`].
/// - anything else (pending / failed / done-but-file-missing) →
///   [`IsolationInput::WaitForStems`].
///
/// Keyed on the RAW status (not the collapsed display `StemsState`, which folds
/// in instrumental-file existence) so the mtl step depends only on the ONE
/// track it consumes — the vocals.
pub(crate) fn isolation_input(
    stem_status: Option<&str>,
    vocals_path: &std::path::Path,
    vocals_exists: bool,
) -> IsolationInput {
    // RED (#144): the 'done'/else arms are SWAPPED so the `isolation_input_*`
    // tests fail; the GREEN commit un-swaps them to the real decision. Every
    // variant is still constructed and every field read, so the RED tree stays
    // clippy `-D warnings` clean (no dead_code / unused) — the no-compile-box
    // RED pattern from `.claude/rules/rust-workspace.md`.
    match stem_status {
        Some("unsupported") => IsolationInput::BaseTierOnly,
        Some("done") if vocals_exists => IsolationInput::WaitForStems,
        _ => IsolationInput::Stems(vocals_path.to_path_buf()),
    }
}

// ---------------------------------------------------------------------------
// Live-handle seam for the lyrics worker — reads its in-process wall handles to
// drive the abort watcher, and surfaces the abort to the dashboard. I/O only;
// the pure decision is `AbortPolicy` above. Kept here (not worker.rs) for the
// 1000-line cap.
// ---------------------------------------------------------------------------

impl crate::lyrics::worker::LyricsWorker {
    /// Run `fut` (a heavy GPU-subprocess future) under the #161 abort watcher,
    /// sampling THIS worker's live wall handles each second. Thin wrapper over
    /// [`run_with_wall_abort`] so the two lyrics call sites (isolation + mtl)
    /// share one wiring.
    #[cfg_attr(test, mutants::skip)]
    pub(crate) async fn wall_abort<T>(
        &self,
        fut: impl std::future::Future<Output = T>,
        gate_enabled: bool,
    ) -> Result<T, WallAbort> {
        run_with_wall_abort(fut, gate_enabled, || self.wall_activity()).await
    }

    /// Surface a mid-job wall-abort to the dashboard: log the once-per-transition
    /// "waiting — wall in use" line and set the same song-less waiting badge the
    /// pre-flight gate uses (`enter_wall_wait`), so a mid-song abort does not
    /// flicker the dashboard between a song-named stage and the generic badge.
    #[cfg_attr(test, mutants::skip)]
    pub(crate) async fn enter_wall_abort(&self, detail: &str) {
        self.note_wall_gate(true, detail);
        self.enter_wall_wait(detail).await;
    }

    /// Vocal isolation (`aligner::preprocess_vocals`) under the #161 abort
    /// watcher. `Ok(Some(wav))` on success, `Ok(None)` when isolation is not
    /// applicable (no venv python / no audio file) or the subprocess failed
    /// normally (best-effort, same as before #161), and `Err(WallAbort)` when
    /// the wall went busy mid-run — in which case the partial WAV is deleted so
    /// the next idle pick re-isolates from scratch to identical bytes. Extracted
    /// from `process_song` (worker.rs 1000-line cap).
    #[cfg_attr(test, mutants::skip)]
    pub(crate) async fn isolate_vocals(
        &self,
        row: &crate::db::models::VideoLyricsRow,
        gpu_mem: Option<&str>,
        plan: &crate::lyrics::heavy_plan::HeavyStepPlan,
        abort_enabled: bool,
    ) -> Result<Option<PathBuf>, WallAbort> {
        let venv_python = self.venv_python.read().await.clone();
        let (Some(python), Some(audio_path)) = (
            venv_python.as_ref(),
            row.audio_file_path.as_ref().map(PathBuf::from),
        ) else {
            return Ok(None);
        };
        if !audio_path.exists() {
            return Ok(None);
        }
        let wav_path = self
            .cache_dir
            .join(format!("{}_vocals16k.wav", row.youtube_id));
        // #171: resumable per-segment scratch dir next to the cache. Preserved
        // across a stall/kill so the next pick resumes from finished segments.
        let work_dir = self.cache_dir.join(format!("{}_isolation", row.youtube_id));
        let iso_fut = crate::lyrics::aligner::preprocess_vocals(
            python,
            &self.script_path,
            &self.models_dir,
            &audio_path,
            &wav_path,
            &work_dir,
            isolation_step_timeout(plan, row.duration_ms),
            gpu_mem,
            plan,
        );
        // #162: the abort watcher is armed ONLY for a GPU-mode job
        // (`abort_enabled`). A CPU/IDLE job is never aborted — it cannot disturb
        // the wall — so it runs to completion (`wall_abort` with the flag off
        // simply awaits the future).
        match self.wall_abort(iso_fut, abort_enabled).await {
            Ok(Ok(p)) => Ok(Some(p)),
            Ok(Err(e)) => {
                tracing::warn!("worker: vocal isolation failed for {}: {e}", row.youtube_id);
                Ok(None)
            }
            Err(abort) => {
                // #161: wall became busy mid-isolation. A genuine cache hit
                // returns before the first 1 s poll, so this only ever deletes
                // an INCOMPLETE WAV; delete it so the cache-hit guard (>1 MB)
                // does not later reuse a truncated file.
                let _ = tokio::fs::remove_file(&wav_path).await;
                tracing::info!(
                    "worker: aborted heavy step — wall became busy ({}) during vocal isolation of {}",
                    abort.detail,
                    row.youtube_id
                );
                Err(abort)
            }
        }
    }
}

#[cfg(test)]
#[path = "idle_gate_abort_tests.rs"]
mod tests;
