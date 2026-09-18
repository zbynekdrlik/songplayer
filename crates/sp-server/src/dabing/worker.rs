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

/// The bundled ffmpeg path (next to the other tools). Mirrors
/// `tools::ffmpeg_filename` without depending on its visibility.
fn ffmpeg_path(tools_dir: &Path) -> PathBuf {
    let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    tools_dir.join(name)
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

        // Precondition: both stems must exist (the ambient bed + original-voice
        // channels the 4-stream mix needs). While they are missing, park at
        // `stems` and (re)raise the stems manual-priority so separation runs ahead.
        if !models_dabing::dub_stems_ready(
            job.vocals_file_path.as_deref(),
            job.instrumental_file_path.as_deref(),
        ) {
            if job.dub_status != "stems" {
                let _ = models_dabing::mark_dub_waiting_stems(&self.pool, job.video_id).await;
                info!(
                    video_id = job.video_id,
                    "dub worker: waiting for stems (raised stem_manual_priority)"
                );
            }
            return;
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

        // Stems ready → advance to synth.
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
                warn!(video_id = job.video_id, "dub worker: gemini_api_key not set — deferring");
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

        // Memory-headroom guard BEFORE the heavy child (owner's order). Below the
        // floor → leave the row at `synth` with NO backoff; re-picked next tick.
        if crate::lyrics::heavy_slot::heavy_step_memory_defers("dub live-translate") {
            return;
        }

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
                    Err(e) => warn!(%e, video_id = job.video_id, "dub worker: mark_dub_ready failed"),
                }
            }
            Err(e) => {
                let msg = format!("{e:#}");
                warn!(video_id = job.video_id, "dub worker: synthesis failed: {msg}");
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

        // 1. Chunk plan from ffmpeg silencedetect (BELOW_NORMAL, off the heavy slot
        //    — a light one-time pass), parsed + planned by the pure chunk_plan.
        let stderr = self.run_silencedetect(&audio_path).await?;
        let (detected_total, silences) = chunk_plan::parse_silencedetect(&stderr);
        let total_ms = detected_total
            .or_else(|| job.duration_ms.map(|d| d.max(0) as u64))
            .filter(|&t| t > 0)
            .ok_or_else(|| anyhow::anyhow!("dub: could not determine audio duration"))?;
        let chunks = chunk_plan::plan_chunks(&silences, total_ms, &ChunkPlanConfig::default());
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
        let activity =
            wall_activity_from(self.ndi_health_registry.as_ref(), self.obs_state.as_ref()).await;
        let plan = HeavyStepPlan::for_activity(
            crate::lyrics::heavy_plan::ProcessingMode::LowPriority,
            activity,
        );
        info!(
            video_id = job.video_id,
            pace,
            mode = plan.label(),
            "dub worker: starting live-translate ({} chunks, ~{}s audio)",
            chunks.len(),
            total_ms / 1000
        );
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
        let child = cmd.spawn().context("failed to spawn ffmpeg silencedetect")?;
        // 20-minute ceiling — decoding a long file, but never the whole stream.
        let out = tokio::time::timeout(
            Duration::from_secs(1200),
            child.wait_with_output(),
        )
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

    /// Materialise `dub_worker.py` into `tools_dir` (embedded at compile time),
    /// rewriting only when stale. Mirrors `StemWorker::ensure_script`.
    async fn ensure_script(&self) -> anyhow::Result<PathBuf> {
        const EMBEDDED: &str = include_str!("../../../../scripts/dub_worker.py");
        if let Some(parent) = self.script_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let stale = match tokio::fs::read_to_string(&self.script_path).await {
            Ok(existing) => existing != EMBEDDED,
            Err(_) => true,
        };
        if stale {
            tokio::fs::write(&self.script_path, EMBEDDED).await?;
            info!("dub_worker: wrote {}", self.script_path.display());
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
    fn dub_eta_is_audio_plus_drain() {
        assert_eq!(dub_eta(60_000), Duration::from_secs(180));
    }

    #[test]
    fn ffmpeg_path_is_under_tools_dir() {
        let p = ffmpeg_path(Path::new("/c/tools"));
        assert!(p.starts_with("/c/tools"));
        assert!(p.file_name().unwrap().to_string_lossy().starts_with("ffmpeg"));
    }
}
