//! Background dub-synthesis worker (#183 D4).
//!
//! Mirrors `stems/worker.rs`: a 10 s tick that, for the next dub-requested,
//! downloaded video, runs the Gemini Live Translate child under the shared heavy
//! slot at BELOW_NORMAL priority (never gating playback — the owner's "processing
//! keeps running during playback at reduced priority" rule). #184 round H step 2:
//! the child streams the whole video's audio (the vocals stem when it is ready,
//! else the original — [`dub_input_audio`]) through ONE continuous Live session
//! with the model + voice from the `dub_model` / `dub_voice` settings; the worker
//! logs the child's session stats and records `dub_status = ready`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use sqlx::SqlitePool;
use tokio::sync::{RwLock, broadcast};
use tracing::{info, warn};

use crate::db::models_dabing;
use crate::lyrics::heavy_plan::HeavyStepPlan;
use crate::lyrics::idle_gate::{startup_floor_defers, wall_activity_from};

/// How often the worker looks for the next dub job.
const TICK: Duration = Duration::from_secs(10);

/// Backoff after a failed dub attempt before the row is retried.
const DUB_BACKOFF: Duration = Duration::from_secs(300);

/// The heavy-child wall-clock ETA used only for the stall-wait log line: the
/// audio duration (real-time streaming) plus generous drain headroom.
fn dub_eta(total_ms: u64) -> Duration {
    Duration::from_millis(total_ms) + Duration::from_secs(120)
}

pub struct DubWorker {
    pool: SqlitePool,
    tools_dir: PathBuf,
    script_path: PathBuf,
    ndi_health_registry: Option<Arc<crate::playback::ndi_health::NdiHealthRegistry>>,
    obs_state: Option<Arc<RwLock<crate::obs::ObsState>>>,
    warned_no_python: AtomicBool,
    /// Set once `google-genai` has been confirmed importable in the venv, so the
    /// idempotent probe/install runs at most once per process (retried on failure).
    genai_ready: AtomicBool,
    /// Set once the #182 startup subtitle backfill has run (once per process).
    subtitles_backfilled: AtomicBool,
}

/// Parse the `dub_worker_enabled` setting. Default ON so a fresh deploy processes
/// dub requests; `false`/`0`/`off`/`no` disable it. Mirrors `stem_worker_enabled`.
pub fn worker_enabled(raw: Option<&str>) -> bool {
    match raw {
        None => true,
        Some(v) => {
            let v = v.trim().to_ascii_lowercase();
            !(v == "false" || v == "0" || v == "off" || v == "no")
        }
    }
}

/// Parse the `dub_voice` setting (#184 round H step 2). Absent/blank or
/// `speaker` in any case → `speaker` (the speaker's own voice, no
/// `speech_config` — the child reads it case-insensitively too); any other
/// non-blank value is trimmed and passed through as a prebuilt voice name (the
/// catalogue is not enforced here, so a new voice needs no code change). Pure —
/// unit-tested.
pub fn dub_voice_from(raw: Option<&str>) -> String {
    use sp_core::config::{DEFAULT_DUB_VOICE, DUB_VOICE_SPEAKER};
    match raw.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) if v.eq_ignore_ascii_case(DUB_VOICE_SPEAKER) => DUB_VOICE_SPEAKER.to_string(),
        Some(v) => v.to_string(),
        None => DEFAULT_DUB_VOICE.to_string(),
    }
}

/// Parse the `dub_model` setting (#184 round H step 2) — the Live Translate model
/// the dub session uses. Absent/blank → [`sp_core::config::DEFAULT_DUB_MODEL`];
/// any non-blank id is trimmed and passed through, so upgrading to a newer model
/// is a setting change. Pure — unit-tested.
pub fn dub_model_from(raw: Option<&str>) -> String {
    match raw.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => v.to_string(),
        None => sp_core::config::DEFAULT_DUB_MODEL.to_string(),
    }
}

/// The audio the dub session translates (#184 round H step 2): the video's
/// VOCALS stem when the stems worker finished (`stem_status = 'done'`), the DB
/// carries its path and the file exists — a cleaner input at no extra cost —
/// else the normalized ORIGINAL. Pure — unit-tested (the caller checks the file).
pub fn dub_input_audio(
    original: &str,
    vocals: Option<&str>,
    stem_status: Option<&str>,
    vocals_exists: bool,
) -> PathBuf {
    match vocals {
        Some(v) if !v.is_empty() && stem_status == Some("done") && vocals_exists => {
            PathBuf::from(v)
        }
        _ => PathBuf::from(original),
    }
}

/// The Python tool scripts the dub worker materialises into `tools_dir` (embedded
/// at compile time): the worker itself PLUS the modules it imports at load —
/// `dub_live_session.py` (#184 round H step 2, the ONE continuous Live session),
/// `dub_loudness.py` (#184 round F, the loudness rules of the assembly) and
/// `win_replace.py` (#184 round F2, the POSIX-semantics rename that promotes the
/// finished dub even while SongPlayer holds it open). Pure — unit-tested so a
/// helper module can never silently stop shipping to the box.
fn embedded_tool_scripts() -> [(&'static str, &'static str); 4] {
    [
        (
            "dub_worker.py",
            include_str!("../../../../scripts/dub_worker.py"),
        ),
        (
            "dub_live_session.py",
            include_str!("../../../../scripts/dub_live_session.py"),
        ),
        (
            "dub_loudness.py",
            include_str!("../../../../scripts/dub_loudness.py"),
        ),
        (
            "win_replace.py",
            include_str!("../../../../scripts/win_replace.py"),
        ),
    ]
}

impl DubWorker {
    pub fn new(
        pool: SqlitePool,
        tools_dir: PathBuf,
        ndi_health_registry: Arc<crate::playback::ndi_health::NdiHealthRegistry>,
        obs_state: Arc<RwLock<crate::obs::ObsState>>,
    ) -> Self {
        let script_path = tools_dir.join("dub_worker.py");
        Self {
            pool,
            tools_dir,
            script_path,
            ndi_health_registry: Some(ndi_health_registry),
            obs_state: Some(obs_state),
            warned_no_python: AtomicBool::new(false),
            genai_ready: AtomicBool::new(false),
            subtitles_backfilled: AtomicBool::new(false),
        }
    }

    pub async fn run(self, mut shutdown_rx: broadcast::Receiver<()>) {
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        info!("dub worker started");
        loop {
            tokio::select! {
                _ = interval.tick() => self.process_next().await,
                _ = shutdown_rx.recv() => {
                    info!("dub worker shutting down");
                    break;
                }
            }
        }
    }

    async fn process_next(&self) {
        // Operational kill-switch, read live each tick.
        let enabled = crate::db::models::get_setting(&self.pool, "dub_worker_enabled")
            .await
            .ok()
            .flatten();
        if !worker_enabled(enabled.as_deref()) {
            return;
        }

        // #182: once per process, give every finished dub that still lacks the
        // subtitle track (finished before D3 shipped) its EN/SK subtitles.
        if !self.subtitles_backfilled.swap(true, Ordering::Relaxed) {
            crate::dabing::subtitles_store::backfill_missing_subtitles(&self.pool).await;
        }

        // #229 item C: a dub calls Gemini Live-Translate — paid AI.
        if !self.may_dub().await {
            return;
        }

        let python = crate::lyrics::bootstrap::venv_python_path(&self.tools_dir);
        if !python.exists() {
            if !self.warned_no_python.swap(true, Ordering::Relaxed) {
                warn!(
                    "dub worker: venv python not found at {python:?} — dub waits for the lyrics bootstrap"
                );
            }
            return;
        }

        let job = match models_dabing::get_next_dub_job(&self.pool).await {
            Ok(Some(j)) => j,
            Ok(None) => return,
            Err(e) => {
                warn!(%e, "dub worker: selection query failed");
                return;
            }
        };

        // #183 round 2: the dub chain NEVER waits for stems — long videos the stem
        // worker cannot separate (over the 120-min cap) must still be dubbed. The
        // pure `synth_ready` decides: proceed now; if the stems are merely pending
        // (absent but within the cap) raise their manual priority so a later
        // separation enriches the mix (2-stream → 4-stream on the next open),
        // without parking here.
        let stems = models_dabing::dub_stems_state(
            job.vocals_file_path.as_deref(),
            job.instrumental_file_path.as_deref(),
            job.stem_status.as_deref(),
        );
        match models_dabing::synth_ready(true, stems, job.duration_ms) {
            models_dabing::SynthDecision::WaitForDownload => {
                // Defensive — `get_next_dub_job` already requires normalized+audio.
                return;
            }
            models_dabing::SynthDecision::ProceedRaisePriority => {
                // Raise the stems priority once, when first leaving the pre-synth
                // state (the next tick sees `dub_status = 'synth'` and skips it).
                if job.dub_status != "synth" {
                    let _ = models_dabing::raise_dub_stem_priority(&self.pool, job.video_id).await;
                    info!(
                        video_id = job.video_id,
                        "dub worker: stems pending — raised stem_manual_priority, proceeding to synth without waiting"
                    );
                }
            }
            models_dabing::SynthDecision::Proceed => {}
        }

        // #167 startup floor: no heavy step in the first 60 s so the wall pipelines
        // come up on a quiet box; the row stays pending, re-picked next tick.
        if let Some(reg) = self.ndi_health_registry.as_ref()
            && startup_floor_defers(reg.since_created())
        {
            info!(
                video_id = job.video_id,
                "dub worker: heavy step deferred (startup grace)"
            );
            return;
        }

        // Advance to synth (stems ready, unsupported, or pending-but-not-waited).
        if job.dub_status != "synth"
            && let Err(e) = models_dabing::mark_dub_synth(&self.pool, job.video_id).await
        {
            warn!(%e, video_id = job.video_id, "dub worker: mark_dub_synth failed");
            return;
        }

        // Gemini key (first entry, rotation-order preserved) — env-only for the child.
        let Some(key) = self.job_key(&job).await else {
            return;
        };

        let script_path = match self.ensure_script().await {
            Ok(p) => p,
            Err(e) => {
                warn!(%e, "dub worker: could not materialise dub_worker.py — deferring");
                return;
            }
        };

        // Ensure the google-genai SDK is importable in the venv (idempotent, light
        // — never triggers the heavy torch reinstall). Once per process.
        if !self.genai_ready.load(Ordering::Relaxed) {
            match crate::lyrics::bootstrap::ensure_genai(&python).await {
                Ok(v) => {
                    info!("dub worker: google-genai ready (v{v})");
                    self.genai_ready.store(true, Ordering::Relaxed);
                }
                Err(e) => {
                    warn!(%e, "dub worker: google-genai not available — deferring");
                    return;
                }
            }
        }

        // #144 r2: signal the dub-priority want, then QUEUE for the heavy slot
        // (fair FIFO — block behind a running child) and measure headroom AT
        // SPAWN with the permit held. The want is published BEFORE queueing so a
        // running separation yields the slot; `acquire_slot_for_spawn` clears it
        // the instant the dub acquires (in `heavy_slot::acquire_on`). Below the
        // floor → release the permit and leave the row at `synth` with NO backoff
        // (never a `record_dub_deferral`); re-picked next tick. Both guards are
        // held across `synthesize` — the deep acquire in
        // `dabing::child::run_live_translate` is gone (a second acquire on the
        // same task would deadlock the Semaphore(1)).
        let _dub_want = crate::lyrics::heavy_slot::dub_slot_want_guard();
        let _slot =
            match crate::lyrics::heavy_slot::acquire_slot_for_spawn("dub live-translate").await {
                Ok(g) => g,
                Err(_) => return,
            };

        // #136: synthesize on the input the row records NOW (the audio, and the
        // vocals stem `dub_input_audio` may pick) — a rename (the metadata
        // repair) while this job waited for the slot moved them. No audio on
        // disk = a no-penalty re-pick later (`song_input::job_input`).
        let Some(input) = crate::song_input::job_input(
            &self.pool,
            job.video_id,
            &job.audio_file_path,
            crate::song_input::HeavyJob::Dub,
        )
        .await
        else {
            return;
        };
        let job = input.dub_job(job);
        let outcome = self.synthesize(&python, &script_path, &key, &job).await;
        match outcome {
            Ok(out_path) => self.record_dub_ready(&job, &out_path).await,
            Err(e) => {
                let msg = format!("{e:#}");
                warn!(
                    video_id = job.video_id,
                    "dub worker: synthesis failed: {msg}"
                );
                let _ =
                    models_dabing::record_dub_deferral(&self.pool, job.video_id, &msg, DUB_BACKOFF)
                        .await;
            }
        }
    }

    /// Record a finished dub (`out_path`, named after the audio the job started
    /// with) as `ready`, then re-link the song (#136): a rename while the job ran
    /// (the metadata repair) left the dub + transcripts under the old name, and
    /// the dub mixer looks for them under the audio's CURRENT name.
    async fn record_dub_ready(&self, job: &models_dabing::DubJob, out_path: &Path) {
        match models_dabing::mark_dub_ready(
            &self.pool,
            job.video_id,
            &out_path.to_string_lossy(),
            crate::dabing::DUB_ENGINE,
        )
        .await
        {
            Ok(()) => info!(
                video_id = job.video_id,
                dub = %out_path.display(),
                "dub worker: ready"
            ),
            Err(e) => {
                warn!(%e, video_id = job.video_id, "dub worker: mark_dub_ready failed")
            }
        }
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
            warn!(%e, video_id = job.video_id, "dub worker: dub re-link failed");
        }
    }

    /// Run the whole synthesis for one job: resolve input/model/voice → the ONE
    /// continuous-session child → post-checks + D3 subtitles. Returns the dub
    /// file path on success.
    async fn synthesize(
        &self,
        python: &Path,
        script_path: &Path,
        key: &str,
        job: &models_dabing::DubJob,
    ) -> anyhow::Result<PathBuf> {
        let audio_path = PathBuf::from(&job.audio_file_path);
        let out_path = crate::stems::dub_path(&audio_path);
        let transcripts_path = crate::stems::dub_transcripts_path(&audio_path);
        let work_dir = audio_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(format!("{}_dub", job.youtube_id));
        tokio::fs::create_dir_all(&work_dir).await.ok();

        // #184 round H step 2: the session input — the vocals stem when ready.
        let vocals_exists = match job.vocals_file_path.as_deref() {
            Some(v) if !v.is_empty() => tokio::fs::try_exists(v).await.unwrap_or(false),
            _ => false,
        };
        let input = dub_input_audio(
            &job.audio_file_path,
            job.vocals_file_path.as_deref(),
            job.stem_status.as_deref(),
            vocals_exists,
        );
        let model = dub_model_from(
            crate::db::models::get_setting(&self.pool, sp_core::config::SETTING_DUB_MODEL)
                .await
                .ok()
                .flatten()
                .as_deref(),
        );
        // Persist the resolved voice in the repurposed `dub_voice_ref_path`
        // column so the dashboard shows it; a persist error is non-fatal.
        let voice = dub_voice_from(
            crate::db::models::get_setting(&self.pool, sp_core::config::SETTING_DUB_VOICE)
                .await
                .ok()
                .flatten()
                .as_deref(),
        );
        if let Err(e) = models_dabing::set_dub_voice(&self.pool, job.video_id, &voice).await {
            warn!(%e, video_id = job.video_id, "dub worker: set_dub_voice failed (non-fatal)");
        }
        let activity =
            wall_activity_from(self.ndi_health_registry.as_ref(), self.obs_state.as_ref()).await;
        let plan = HeavyStepPlan::for_activity(
            crate::lyrics::heavy_plan::ProcessingMode::LowPriority,
            activity,
        );
        // #203: publish the live containment for the dub child (CPU cap +
        // affinity + memory priority), applied by the shared Job Object seam.
        crate::lyrics::heavy_slot::refresh_containment(&self.pool).await;
        let total_ms = job.duration_ms.unwrap_or(0).max(0) as u64;
        info!(
            video_id = job.video_id,
            input = %input.display(),
            %model,
            %voice,
            mode = plan.label(),
            "dub worker: starting live-translate (one continuous session, ~{}s audio)",
            total_ms / 1000
        );
        // #144 r2: the dub-priority want + the heavy slot are taken by the caller
        // (`process_next`) BEFORE `synthesize`, and held across it — see there.
        let summary = crate::dabing::child::run_live_translate(
            python,
            script_path,
            &input,
            &out_path,
            &transcripts_path,
            &work_dir,
            key,
            &model,
            &voice,
            dub_eta(total_ms),
            &plan,
        )
        .await?;
        let st = &summary.session;
        info!(
            video_id = job.video_id,
            connections = ?st.connections,
            reconnects = ?st.reconnects,
            output_to_input = ?st.output_to_input_ratio,
            max_voiced_gap_s = ?st.max_voiced_gap_s,
            latency_ms = ?st.latency_ms,
            drain = ?st.drain_end_reason,
            "dub worker: live-translate session done"
        );

        // Post-condition: the dub file must exist and be non-trivial.
        let out = PathBuf::from(&summary.out_path);
        let meta = tokio::fs::metadata(&out)
            .await
            .with_context(|| format!("dub: produced no {}", out.display()))?;
        if meta.len() < 1_000 {
            anyhow::bail!("dub: produced a suspiciously small {}", out.display());
        }

        // D3 (#182): the dub audio is finalized — build EN/SK subtitles from the
        // Live-session transcripts and store them as the video's lyrics track
        // (so the wall renders them), BEFORE the caller marks dub_status = ready.
        // A subtitle failure must NEVER fail the dub: it is logged and swallowed.
        let cache_dir = audio_path.parent().unwrap_or_else(|| Path::new("."));
        match crate::dabing::subtitles_store::build_and_store_subtitles(
            &self.pool,
            cache_dir,
            &job.youtube_id,
            job.video_id,
            &transcripts_path,
        )
        .await
        {
            Ok(0) => info!(
                video_id = job.video_id,
                "dub worker: transcript had no usable subtitles"
            ),
            Ok(n) => info!(
                video_id = job.video_id,
                lines = n,
                "dub worker: stored EN/SK subtitles"
            ),
            Err(e) => warn!(
                %e,
                video_id = job.video_id,
                "dub worker: subtitle build failed (dub unaffected)"
            ),
        }

        Ok(out)
    }

    /// The Gemini key `job`'s Live-Translate child gets (the first of the
    /// list), or `None`: with no key set the job is deferred (an attempt and
    /// `dub_error`, `record_dub_deferral`); with paid AI switched off since
    /// the tick's `may_dub` it is held where it is, never failed (#229 item
    /// C, review round 13).
    async fn job_key(&self, job: &models_dabing::DubJob) -> Option<String> {
        if let Some(key) = self.first_gemini_key().await {
            return Some(key);
        }
        if !crate::paid_ai::enabled(&self.pool).await {
            crate::paid_ai::hold(crate::paid_ai::Held::Dub, &job.youtube_id);
            return None;
        }
        warn!(
            video_id = job.video_id,
            "dub worker: gemini_api_key not set — deferring"
        );
        let _ = models_dabing::record_dub_deferral(
            &self.pool,
            job.video_id,
            "gemini_api_key not set",
            DUB_BACKOFF,
        )
        .await;
        None
    }

    /// First `gemini_api_key` CSV entry (rotation-order preserved), or `None` when
    /// the setting is unset/empty — or while this node's paid AI is off (#229
    /// item C: the key is read through `paid_ai::gemini_keys`).
    async fn first_gemini_key(&self) -> Option<String> {
        crate::paid_ai::gemini_keys(&self.pool)
            .await?
            .into_iter()
            .next()
    }

    /// #229 item C: whether a dub may run now — only while this node's paid
    /// AI is on (`paid_ai::enabled`, read every tick). While it is off every
    /// dub job waits where it is: no attempt, no failure mark; the job that
    /// would run now is held (`paid_ai::hold`, one INFO per video), none
    /// when there is no job.
    async fn may_dub(&self) -> bool {
        if crate::paid_ai::enabled(&self.pool).await {
            return true;
        }
        if let Ok(Some(job)) = models_dabing::get_next_dub_job(&self.pool).await {
            crate::paid_ai::hold(crate::paid_ai::Held::Dub, &job.youtube_id);
        }
        false
    }

    /// Materialise the dub tool scripts into `tools_dir` (embedded at compile
    /// time) through the shared `embedded_scripts::materialise`, rewriting only
    /// the stale ones, like `StemWorker::ensure_script`; ships `dub_worker.py`
    /// plus the modules it imports: `dub_live_session.py` (the round-H
    /// continuous session), `dub_loudness.py` (the round-F loudness-matched
    /// assembly) and `win_replace.py` (the round-F2 POSIX rename of the
    /// finished dub). Returns the worker script path.
    async fn ensure_script(&self) -> anyhow::Result<PathBuf> {
        let scripts = embedded_tool_scripts();
        crate::embedded_scripts::materialise(&self.tools_dir, &scripts, "dub_worker").await?;
        Ok(self.script_path.clone())
    }
}

#[cfg(test)]
#[path = "worker_tests.rs"]
mod tests;
