//! Reference stage (#143, v21) — the forced-alignment ★ tier.
//!
//! Runs the mtl-align → Gemini-ASR → gate chain for one song's chosen
//! candidate text. All I/O is behind the injected `ReferenceStageBackend`;
//! everything else here is pure decision logic. On gate PASS the caller
//! (`worker_reference::run_mtl_reference_stage`) ships the mtl line timings
//! directly (★); on FAIL/ERROR/skip the song falls through to the v22 g35t
//! base tier (`worker_g35t`).
//!
//! The v20 `Orchestrator` WhisperX tier chain that used to live here was
//! deleted in #159 (one-regime cleanup) along with the `AlignmentBackend`
//! trait; only the reference stage survives, so the file name is historical.

use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Lever 2 (#143) — forced-alignment reference stage.
//
// Aligns the chosen text candidate's lines to the isolated vocal stem with
// `lyrics-alignment-mtl` (`mtl_aligner::align`), verifies the result against an
// independent Gemini 3.5 Transcribe word transcript
// (`g35t_client::transcribe_words` + `reference_gate::evaluate`), and on PASS
// ships the mtl line timings directly.
//
// `ReferenceStageBackend` is the injection seam: production wires
// `RealReferenceStageBackend` (real subprocess + real HTTP); tests inject a
// fake so the gate-decision logic in `run_reference_stage` is exercised
// without spawning a process or making a network call.
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
pub trait ReferenceStageBackend: Send + Sync {
    async fn mtl_align(
        &self,
        vocals_wav: &Path,
        video_id: &str,
        lines: &[String],
    ) -> anyhow::Result<crate::lyrics::mtl_aligner::MtlOutput>;

    async fn asr_transcribe(
        &self,
        vocals_wav: &Path,
    ) -> anyhow::Result<Vec<crate::lyrics::g35t_client::AsrWord>>;
}

/// Production `ReferenceStageBackend`: real `mtl_aligner::align` subprocess
/// call + real `g35t_client::transcribe_words` HTTP call.
pub struct RealReferenceStageBackend {
    pub mtl_cfg: crate::lyrics::mtl_aligner::MtlConfig,
    pub work_dir: PathBuf,
    pub http_client: reqwest::Client,
    pub gemini_keys: Vec<String>,
    /// Raw `lyrics_gpu_mem_fraction` DB setting (#154), passed to the mtl
    /// subprocess as the VRAM cap. `None` → the child applies the default.
    pub gpu_mem_setting: Option<String>,
    /// #161 mid-job wall-abort handles: the engine health registry + OBS state
    /// this backend samples (once per second) to KILL the mtl subprocess when
    /// the wall becomes busy mid-align. `None` in unit tests (they inject a fake
    /// backend, so this real backend's abort wrapper is never exercised there).
    pub ndi_health_registry: Option<std::sync::Arc<crate::playback::ndi_health::NdiHealthRegistry>>,
    pub obs_state: Option<std::sync::Arc<tokio::sync::RwLock<crate::obs::ObsState>>>,
    /// #162: the processing mode (read once per song by the worker, passed in).
    /// Selects the mtl device/priority plan and whether the abort watcher is
    /// armed (GPU jobs only — a CPU/IDLE job is never aborted).
    pub mode: crate::lyrics::heavy_plan::ProcessingMode,
}

#[async_trait::async_trait]
impl ReferenceStageBackend for RealReferenceStageBackend {
    // #161: wraps the mtl subprocess in the wall-abort watcher — integration-
    // tested only (needs a live GPU subprocess), so skip mutation.
    #[cfg_attr(test, mutants::skip)]
    async fn mtl_align(
        &self,
        vocals_wav: &Path,
        video_id: &str,
        lines: &[String],
    ) -> anyhow::Result<crate::lyrics::mtl_aligner::MtlOutput> {
        use crate::lyrics::heavy_plan::{HeavyStepPlan, ProcessingMode};

        let activity = crate::lyrics::idle_gate::wall_activity_from(
            self.ndi_health_registry.as_ref(),
            self.obs_state.as_ref(),
        )
        .await;
        let plan = HeavyStepPlan::for_activity(self.mode, activity);
        tracing::info!(
            "lyrics_worker: heavy step mtl mode={} (wall {})",
            plan.label(),
            activity.reason().unwrap_or("idle")
        );

        // #162: the mtl align (a 2–5 min subprocess) runs under the chosen plan.
        // The abort watcher is armed ONLY for a GPU-mode job (a CPU/IDLE job
        // cannot disturb the wall, so it is never aborted). On a GPU abort in
        // low-priority mode we re-run mtl IMMEDIATELY on CPU (no defer); in
        // idle-only mode we surface the abort so the whole song defers.
        if !plan.is_gpu() {
            // CPU/IDLE plan: never watched, runs to completion.
            return crate::lyrics::mtl_aligner::align(
                &self.mtl_cfg,
                vocals_wav,
                video_id,
                lines,
                &self.work_dir,
                self.gpu_mem_setting.as_deref(),
                &plan,
            )
            .await;
        }

        // GPU plan: watch the wall and kill the child if it goes busy.
        let align_fut = crate::lyrics::mtl_aligner::align(
            &self.mtl_cfg,
            vocals_wav,
            video_id,
            lines,
            &self.work_dir,
            self.gpu_mem_setting.as_deref(),
            &plan,
        );
        match crate::lyrics::idle_gate_abort::run_with_wall_abort(align_fut, true, || {
            crate::lyrics::idle_gate::wall_activity_from(
                self.ndi_health_registry.as_ref(),
                self.obs_state.as_ref(),
            )
        })
        .await
        {
            Ok(inner) => inner,
            Err(abort) => {
                // Delete the aborted step's OWN scratch files (the partial
                // output JSON + the input text JSON `align` wrote); the isolated
                // vocal WAV (a completed intermediate) stays.
                let out_json = self.work_dir.join(format!("{video_id}_mtl_out.json"));
                let text_json = self.work_dir.join(format!("{video_id}_mtl_text.json"));
                let _ = tokio::fs::remove_file(&out_json).await;
                let _ = tokio::fs::remove_file(&text_json).await;
                match self.mode {
                    ProcessingMode::LowPriority => {
                        // #162: re-run mtl on CPU immediately — no defer, no idle
                        // wait. Byte-identical ★ output (same aligner, CPU vs GPU).
                        tracing::info!(
                            "lyrics_worker: heavy step mtl re-run mode=cpu-idle \
                             after GPU abort ({})",
                            abort.detail
                        );
                        crate::lyrics::mtl_aligner::align(
                            &self.mtl_cfg,
                            vocals_wav,
                            video_id,
                            lines,
                            &self.work_dir,
                            self.gpu_mem_setting.as_deref(),
                            &HeavyStepPlan::cpu_idle(),
                        )
                        .await
                    }
                    ProcessingMode::IdleOnly => {
                        // Surface the abort as a downcastable error so
                        // `run_reference_stage` maps it to WallAborted → the song
                        // defers (today's idle-only semantics).
                        Err(anyhow::Error::new(abort))
                    }
                }
            }
        }
    }

    async fn asr_transcribe(
        &self,
        vocals_wav: &Path,
    ) -> anyhow::Result<Vec<crate::lyrics::g35t_client::AsrWord>> {
        crate::lyrics::g35t_client::transcribe_words(
            &self.http_client,
            &self.gemini_keys,
            vocals_wav,
            &["en-US".to_string()],
        )
        .await
    }
}

/// Outcome of `run_reference_stage`. `Error` covers both an `mtl_align` and
/// an `asr_transcribe` transport failure — the caller treats both
/// identically (log + write the audit sidecar + fall through to the g35t base
/// tier); only the message differs.
pub enum ReferenceStageResult {
    Pass {
        lines: Vec<crate::lyrics::backend::AlignedLine>,
        stats: crate::lyrics::reference_gate::GateStats,
    },
    Fail {
        reason: crate::lyrics::reference_gate::GateFailReason,
        stats: crate::lyrics::reference_gate::GateStats,
        mtl_device: String,
        mtl_elapsed_s: f64,
        asr_word_count: usize,
    },
    Error {
        stage: &'static str,
        message: String,
    },
    /// #161: the mtl subprocess was aborted because the wall became busy
    /// mid-align. The caller defers the whole song (WaitingForWall) — it must
    /// NEVER degrade to the g35t base tier (the ★ mtl tier re-runs to identical
    /// output on the next idle pick).
    WallAborted { detail: String },
}

/// Runs the mtl-align → Gemini-ASR → gate chain for one song's chosen
/// candidate text. All I/O is behind the injected `backend`; everything
/// else here is pure decision logic.
pub async fn run_reference_stage(
    backend: &dyn ReferenceStageBackend,
    vocals_wav: &Path,
    video_id: &str,
    lines: &[String],
) -> ReferenceStageResult {
    let mtl = match backend.mtl_align(vocals_wav, video_id, lines).await {
        Ok(m) => m,
        Err(e) => {
            // #161: a wall-abort of the mtl subprocess is NOT a real alignment
            // failure — surface it distinctly so the caller defers the song
            // (WaitingForWall) rather than degrading to the g35t base tier.
            if let Some(abort) = e.downcast_ref::<crate::lyrics::idle_gate_abort::WallAbort>() {
                return ReferenceStageResult::WallAborted {
                    detail: abort.detail.clone(),
                };
            }
            return ReferenceStageResult::Error {
                stage: "mtl_align",
                message: e.to_string(),
            };
        }
    };
    let words = match backend.asr_transcribe(vocals_wav).await {
        Ok(w) => w,
        Err(e) => {
            return ReferenceStageResult::Error {
                stage: "asr_transcribe",
                message: e.to_string(),
            };
        }
    };
    let asr_word_count = words.len();
    let gate_lines: Vec<crate::lyrics::reference_gate::AlignedLine> = mtl
        .lines
        .iter()
        .map(|l| crate::lyrics::reference_gate::AlignedLine {
            text: l.text.clone(),
            start_ms: l.start_ms,
        })
        .collect();
    match crate::lyrics::reference_gate::evaluate(&gate_lines, &words) {
        crate::lyrics::reference_gate::GateVerdict::Pass(stats) => ReferenceStageResult::Pass {
            lines: mtl
                .lines
                .into_iter()
                .map(|l| crate::lyrics::backend::AlignedLine {
                    text: l.text,
                    start_ms: l.start_ms as u32,
                    end_ms: l.end_ms as u32,
                    // Per feedback_line_timing_only.md: never synthesize
                    // word timings; mtl ships line-level timing only.
                    words: None,
                })
                .collect(),
            stats,
        },
        crate::lyrics::reference_gate::GateVerdict::Fail { reason, stats } => {
            ReferenceStageResult::Fail {
                reason,
                stats,
                mtl_device: mtl.device,
                mtl_elapsed_s: mtl.elapsed_s,
                asr_word_count,
            }
        }
    }
}
