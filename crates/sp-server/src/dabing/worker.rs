//! Background dub-synthesis worker (#183 D4).
//!
//! Mirrors `stems/worker.rs`: a 10 s tick that, for the next dub-requested,
//! downloaded video with stems ready, runs the Gemini Live Translate child under
//! the shared heavy slot at BELOW_NORMAL priority (never gating playback — the
//! owner's "processing keeps running during playback at reduced priority" rule).
//! It computes the chunk plan (ffmpeg `silencedetect` → [`chunk_plan::plan_chunks`]),
//! runs the child, cross-checks the per-chunk drift against
//! [`chunk_plan::placement_for`], and records `dub_status = ready`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use sqlx::SqlitePool;
use tokio::process::Command;
use tokio::sync::{RwLock, broadcast};
use tracing::{info, warn};

use crate::dabing::chunk_plan::{self, ChunkPlanConfig};
use crate::db::models_dabing;
use crate::lyrics::heavy_plan::HeavyStepPlan;
use crate::lyrics::idle_gate::{startup_floor_defers, wall_activity_from};

/// How often the worker looks for the next dub job.
const TICK: Duration = Duration::from_secs(10);

/// Backoff after a failed dub attempt before the row is retried.
const DUB_BACKOFF: Duration = Duration::from_secs(300);

/// Max acceptable per-chunk drift; a chunk beyond this is WARN-logged (not fatal —
/// the mix is still played, the owner's ear is the final verdict).
pub const DUB_MAX_DRIFT_MS: i64 = 2_000;

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

/// Parse the `dub_pace` setting — the Live-input pacing factor (`1.0` = real
/// time, the default; `2.0` = twice real time). Clamped to `0.5..=4.0`; a bad
/// value falls back to real time. Pure — unit-tested.
pub fn dub_pace_from(raw: Option<&str>) -> f32 {
    match raw.and_then(|v| v.trim().parse::<f32>().ok()) {
        Some(v) if v.is_finite() && v > 0.0 => v.clamp(0.5, 4.0),
        _ => 1.0,
    }
}

/// Parse the `dub_voice` setting — the pinned Gemini Live Translate output voice
/// (#184 round C). Absent/blank → the catalogue default (`Charon`); any non-blank
/// name is trimmed and passed through (the catalogue is not enforced here, so a
/// future voice needs no code change). Pure — unit-tested.
pub fn dub_voice_from(raw: Option<&str>) -> String {
    match raw.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => v.to_string(),
        None => sp_core::config::DEFAULT_DUB_VOICE.to_string(),
    }
}

/// Parse the `dub_session_max_s` setting — the dub Live-session length cap in
/// SECONDS (#184 round E), returned in MILLISECONDS for `plan_chunks`. Absent /
/// blank / non-numeric → the default ceiling ([`chunk_plan::DUB_SESSION_MAX_MS`]);
/// a numeric value is clamped to `60..=480` seconds so a session is never shorter
/// than a minute nor longer than the old 8-min ceiling. Pure — unit-tested.
pub fn dub_session_max_ms_from(raw: Option<&str>) -> u64 {
    match raw.and_then(|v| v.trim().parse::<u64>().ok()) {
        Some(secs) => secs.clamp(60, 480) * 1000,
        None => chunk_plan::DUB_SESSION_MAX_MS,
    }
}

/// The bundled ffmpeg path (next to the other tools). Mirrors
/// `tools::ffmpeg_filename` without depending on its visibility.
fn ffmpeg_path(tools_dir: &Path) -> PathBuf {
    let name = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    tools_dir.join(name)
}

/// The Python tool scripts the dub worker materialises into `tools_dir` (embedded
/// at compile time): the worker itself PLUS `dub_voice_check.py`, which the child
/// imports for the #184 round-E per-chunk voice-band guard. Pure — unit-tested so
/// the guard's helper module can never silently stop shipping to the box.
fn embedded_tool_scripts() -> [(&'static str, &'static str); 2] {
    [
        (
            "dub_worker.py",
            include_str!("../../../../scripts/dub_worker.py"),
        ),
        (
            "dub_voice_check.py",
            include_str!("../../../../scripts/dub_voice_check.py"),
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
        let key = match self.first_gemini_key().await {
            Some(k) => k,
            None => {
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
                return;
            }
        };

        let script_path = match self.ensure_script().await {
            Ok(p) => p,
            Err(e) => {
                warn!(%e, "dub worker: could not materialise dub_worker.py — deferring");
                return;
            }
        };

        // Ensure the google-genai SDK is importable in the venv (idempotent, light
        // — never triggers the heavy qwen/torch reinstall). Once per process.
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
        // same task would deadlock the Semaphore(1)); the light silencedetect
        // pass inside `synthesize` now runs while this dub holds the slot, so no
        // other heavy child runs alongside it.
        let _dub_want = crate::lyrics::heavy_slot::dub_slot_want_guard();
        let _slot =
            match crate::lyrics::heavy_slot::acquire_slot_for_spawn("dub live-translate").await {
                Ok(g) => g,
                Err(_) => return,
            };

        let outcome = self.synthesize(&python, &script_path, &key, &job).await;
        match outcome {
            Ok(out_path) => {
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
            }
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

    /// Run the whole synthesis for one job: chunk plan → child → drift check.
    /// Returns the dub file path on success.
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

        // 1. Chunk plan from ffmpeg silencedetect (BELOW_NORMAL, a light one-time
        //    pass), parsed + planned by the pure chunk_plan. #144 r2: the caller
        //    (`process_next`) now holds the heavy slot across this, so the pass no
        //    longer overlaps another heavy child.
        let stderr = self.run_silencedetect(&audio_path).await?;
        let (detected_total, silences) = chunk_plan::parse_silencedetect(&stderr);
        let total_ms = detected_total
            .or_else(|| job.duration_ms.map(|d| d.max(0) as u64))
            .filter(|&t| t > 0)
            .ok_or_else(|| anyhow::anyhow!("dub: could not determine audio duration"))?;
        // #184 round E: cap the Live session at the `dub_session_max_s` setting
        // (default 120 s) so the pinned voice does not drift inside a long session.
        let session_max_ms = dub_session_max_ms_from(
            crate::db::models::get_setting(&self.pool, sp_core::config::SETTING_DUB_SESSION_MAX_S)
                .await
                .ok()
                .flatten()
                .as_deref(),
        );
        let plan_cfg = ChunkPlanConfig {
            min_pause_ms: chunk_plan::MIN_PAUSE_MS,
            max_chunk_ms: session_max_ms,
        };
        let chunks = chunk_plan::plan_chunks(&silences, total_ms, &plan_cfg);
        let plan_json_path = work_dir.join("chunk_plan.json");
        tokio::fs::write(&plan_json_path, serde_json::to_vec(&chunks)?).await?;
        info!(
            video_id = job.video_id,
            total_ms,
            silences = silences.len(),
            chunks = chunks.len(),
            "dub worker: chunk plan ready"
        );

        // 2. Run the Live-Translate child under the heavy slot.
        let pace = dub_pace_from(
            crate::db::models::get_setting(&self.pool, "dub_pace")
                .await
                .ok()
                .flatten()
                .as_deref(),
        );
        // #184 round C: resolve + PIN one voice for this video. Persist it in the
        // repurposed `dub_voice_ref_path` column so a resume/re-run reproduces the
        // same voice and the dashboard can show it. A persist error is non-fatal —
        // the synthesis proceeds with the resolved voice regardless.
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
        info!(
            video_id = job.video_id,
            pace,
            mode = plan.label(),
            "dub worker: starting live-translate ({} chunks, ~{}s audio)",
            chunks.len(),
            total_ms / 1000
        );
        // #144 r2: the dub-priority want + the heavy slot are taken by the caller
        // (`process_next`) BEFORE `synthesize`, and held across it — see there.
        // The slot is held for this child's whole lifetime; no acquire here.
        let summary = crate::dabing::child::run_live_translate(
            python,
            script_path,
            &audio_path,
            &out_path,
            &transcripts_path,
            &plan_json_path,
            &work_dir,
            key,
            pace,
            &voice,
            dub_eta(total_ms),
            &plan,
        )
        .await?;

        // 3. Drift cross-check: the pure placement_for is the canonical decision;
        //    log each chunk's drift + warn on the child disagreeing / drift > 2 s.
        for c in &summary.chunks {
            let placement = chunk_plan::placement_for(
                c.chunk_start_ms,
                c.chunk_len_ms(),
                c.out_len_ms,
                c.next_start_ms,
            );
            let drift = chunk_plan::drift_ms(c.chunk_len_ms(), c.out_len_ms);
            if drift.abs() > DUB_MAX_DRIFT_MS {
                warn!(
                    video_id = job.video_id,
                    chunk = c.index,
                    drift_ms = drift,
                    "dub worker: chunk drift exceeds {DUB_MAX_DRIFT_MS} ms"
                );
            } else {
                info!(
                    video_id = job.video_id,
                    chunk = c.index,
                    drift_ms = drift,
                    tempo = c.tempo,
                    "dub worker: chunk placed"
                );
            }
            if (placement.tempo - c.tempo).abs() > 0.05 {
                warn!(
                    video_id = job.video_id,
                    chunk = c.index,
                    expected_tempo = placement.tempo,
                    applied_tempo = c.tempo,
                    "dub worker: child tempo disagrees with placement_for"
                );
            }
        }

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

    /// Run ffmpeg `silencedetect` on `audio` (BELOW_NORMAL, kill-on-drop, bounded)
    /// and return its stderr for parsing. Not on the heavy slot — a light,
    /// one-time decode pass, not the streaming child.
    async fn run_silencedetect(&self, audio: &Path) -> anyhow::Result<String> {
        let ffmpeg = ffmpeg_path(&self.tools_dir);
        let af = format!(
            "silencedetect=noise=-30dB:d={}",
            chunk_plan::MIN_PAUSE_MS as f64 / 1000.0
        );
        let mut cmd = Command::new(&ffmpeg);
        cmd.args(["-hide_banner", "-nostats", "-i"]);
        cmd.arg(audio);
        cmd.arg("-af").arg(&af);
        cmd.args(["-f", "null", "-"]);
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000 | 0x0000_4000); // CREATE_NO_WINDOW | BELOW_NORMAL
        }
        let child = cmd
            .spawn()
            .context("failed to spawn ffmpeg silencedetect")?;
        // 20-minute ceiling — decoding a long file, but never the whole stream.
        let out = tokio::time::timeout(Duration::from_secs(1200), child.wait_with_output())
            .await
            .context("ffmpeg silencedetect timed out")?
            .context("ffmpeg silencedetect wait failed")?;
        Ok(String::from_utf8_lossy(&out.stderr).into_owned())
    }

    /// First `gemini_api_key` CSV entry (rotation-order preserved), or `None` when
    /// the setting is unset/empty.
    async fn first_gemini_key(&self) -> Option<String> {
        let csv = crate::db::models::get_setting(&self.pool, "gemini_api_key")
            .await
            .ok()
            .flatten()?;
        crate::lyrics::g35t_client::gemini_keys_from_setting(&csv)
            .into_iter()
            .next()
    }

    /// Materialise the dub tool scripts into `tools_dir` (embedded at compile
    /// time), rewriting only the stale ones. Mirrors `StemWorker::ensure_script`;
    /// ships `dub_worker.py` AND `dub_voice_check.py` (the child imports the latter
    /// for the round-E voice-band guard). Returns the worker script path.
    async fn ensure_script(&self) -> anyhow::Result<PathBuf> {
        let tools_dir = self.script_path.parent().unwrap_or_else(|| Path::new("."));
        tokio::fs::create_dir_all(tools_dir).await?;
        for (name, content) in embedded_tool_scripts() {
            let path = tools_dir.join(name);
            let stale = match tokio::fs::read_to_string(&path).await {
                Ok(existing) => existing != content,
                Err(_) => true,
            };
            if stale {
                tokio::fs::write(&path, content).await?;
                info!("dub_worker: wrote {}", path.display());
            }
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
        assert!(!worker_enabled(Some("false")));
        assert!(!worker_enabled(Some("0")));
        assert!(!worker_enabled(Some(" OFF ")));
        assert!(!worker_enabled(Some("no")));
    }

    #[test]
    fn dub_pace_defaults_to_real_time_and_clamps() {
        assert!((dub_pace_from(None) - 1.0).abs() < 1e-6);
        assert!((dub_pace_from(Some("2.0")) - 2.0).abs() < 1e-6);
        assert!((dub_pace_from(Some("bad")) - 1.0).abs() < 1e-6);
        assert!((dub_pace_from(Some("0")) - 1.0).abs() < 1e-6);
        assert!((dub_pace_from(Some("9")) - 4.0).abs() < 1e-6); // clamp high
        assert!((dub_pace_from(Some("0.1")) - 0.5).abs() < 1e-6); // clamp low
    }

    #[test]
    fn dub_voice_defaults_to_charon_and_passes_through() {
        // Absent / blank / whitespace-only → the catalogue default.
        assert_eq!(dub_voice_from(None), "Charon");
        assert_eq!(dub_voice_from(Some("")), "Charon");
        assert_eq!(dub_voice_from(Some("   ")), "Charon");
        // Any non-blank name passes through, trimmed.
        assert_eq!(dub_voice_from(Some("Kore")), "Kore");
        assert_eq!(dub_voice_from(Some("  Orus  ")), "Orus");
    }

    #[test]
    fn dub_session_max_ms_defaults_and_clamps() {
        // Absent / blank / non-numeric → the default 2-minute ceiling (round E).
        assert_eq!(dub_session_max_ms_from(None), 120_000);
        assert_eq!(dub_session_max_ms_from(Some("")), 120_000);
        assert_eq!(dub_session_max_ms_from(Some("   ")), 120_000);
        assert_eq!(dub_session_max_ms_from(Some("bad")), 120_000);
        // A valid value is seconds → ms.
        assert_eq!(dub_session_max_ms_from(Some("60")), 60_000);
        assert_eq!(dub_session_max_ms_from(Some(" 90 ")), 90_000);
        // Clamped: below 60 s → 60 s, above 480 s → 480 s.
        assert_eq!(dub_session_max_ms_from(Some("10")), 60_000);
        assert_eq!(dub_session_max_ms_from(Some("999")), 480_000);
    }

    #[test]
    fn dub_eta_is_audio_plus_drain() {
        assert_eq!(dub_eta(60_000), Duration::from_secs(180));
    }

    #[test]
    fn embedded_tool_scripts_ship_worker_and_voice_check() {
        let scripts = embedded_tool_scripts();
        let names: Vec<&str> = scripts.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            vec!["dub_worker.py", "dub_voice_check.py", "dub_loudness.py"]
        );
        for (name, content) in scripts {
            assert!(!content.is_empty(), "{name} embedded empty");
        }
        // The shipped scripts really are the round-E modules the guard needs.
        assert!(
            scripts[0].1.contains("chunk_voice_drift"),
            "dub_worker.py missing the round-E guard helper"
        );
        assert!(
            scripts[1].1.contains("MAX_HIGH_BAND_FRACTION"),
            "dub_voice_check.py is not the round-E high-band module"
        );
        // #184 round F: the worker imports `dub_loudness` for the loudness-matched
        // assembly, so it must ship next to it (a missing module = every dub fails).
        let loudness = scripts
            .iter()
            .find(|(n, _)| *n == "dub_loudness.py")
            .map(|(_, c)| *c)
            .unwrap_or("");
        assert!(
            loudness.contains("def build_loudnorm_second_pass"),
            "dub_loudness.py (round-F loudness rules) is not shipped"
        );
        assert!(
            scripts[0].1.contains("import dub_loudness"),
            "dub_worker.py does not import the shipped dub_loudness module"
        );
    }

    #[test]
    fn ffmpeg_path_is_under_tools_dir() {
        let p = ffmpeg_path(Path::new("/c/tools"));
        assert!(p.starts_with("/c/tools"));
        assert!(
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ffmpeg")
        );
    }
}
