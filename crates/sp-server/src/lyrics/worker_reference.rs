//! `LyricsWorker` extension — Lever-2 (#143) forced-alignment reference stage.
//!
//! Extracted from `worker.rs::process_song` to keep that file under the
//! 1000-line CI limit. `run_mtl_reference_stage` runs BEFORE the
//! WhisperX/asr_path route decides anything: it aligns the best text
//! candidate via mtl, verifies it against Gemini ASR through the reference
//! gate (`orchestrator::run_reference_stage`), and on gate PASS ships the mtl
//! line timings directly while stamping `videos.lyrics_reference`. Gate
//! FAIL/ERROR writes the `{youtube_id}_alignment_audit.json` sidecar and
//! falls through to the existing route unchanged.

use std::path::Path;

use sp_core::lyrics::LyricsTrack;
use tracing::{info, warn};

use super::worker::{LyricsWorker, align_track_to_lyrics_track};
use crate::lyrics::LYRICS_PIPELINE_VERSION;

impl LyricsWorker {
    /// Lever 2 (#143): forced-alignment reference stage. See
    /// `orchestrator::run_reference_stage` for the mtl-align → Gemini-ASR →
    /// gate decision; this wraps it with the skip conditions, the
    /// `videos.lyrics_reference` flag update, and the
    /// `_alignment_audit.json` sidecar on a non-Pass outcome. `backend` is
    /// the injection seam (`orchestrator::ReferenceStageBackend`) —
    /// production passes `RealReferenceStageBackend`, tests pass a fake.
    ///
    /// Returns `Some(LyricsTrack)` on gate PASS — the caller ships it
    /// directly, skipping the WhisperX/replicate route entirely for this
    /// song. Returns `None` on skip/FAIL/ERROR — the caller falls through
    /// to the existing route unchanged.
    pub(crate) async fn run_mtl_reference_stage(
        &self,
        video_id: i64,
        youtube_id: &str,
        best: Option<&crate::lyrics::tier1::CandidateText>,
        clean_vocal: Option<&Path>,
        backend: &dyn crate::lyrics::orchestrator::ReferenceStageBackend,
    ) -> Option<LyricsTrack> {
        const MIN_LINES: usize = 4;

        let mtl_cfg = crate::lyrics::mtl_aligner::MtlConfig::from_tools_dir(&self.tools_dir);
        if !mtl_cfg.is_available() {
            info!(
                youtube_id = %youtube_id,
                "reference_stage: mtl tooling unavailable — skipping (#143)"
            );
            return None;
        }
        let Some(wav) = clean_vocal else {
            info!(youtube_id = %youtube_id, "reference_stage: no vocals wav — skipping");
            return None;
        };
        let Some(best) = best else {
            info!(youtube_id = %youtube_id, "reference_stage: no text candidate — skipping");
            return None;
        };
        if best.lines.len() < MIN_LINES {
            info!(
                youtube_id = %youtube_id,
                lines = best.lines.len(),
                "reference_stage: candidate below the {MIN_LINES}-line floor — skipping"
            );
            return None;
        }

        let outcome =
            crate::lyrics::orchestrator::run_reference_stage(backend, wav, youtube_id, &best.lines)
                .await;

        let audit_ctx = crate::lyrics::audit_ctx::AuditContext {
            cache_dir: &self.cache_dir,
            youtube_id,
        };

        match outcome {
            crate::lyrics::orchestrator::ReferenceStageResult::Pass { lines, stats } => {
                info!(
                    youtube_id = %youtube_id,
                    matched_frac = stats.matched_frac,
                    within_400_frac = stats.within_400_frac,
                    "reference_stage: gate PASS — stamping ★ reference (#143)"
                );
                if let Err(e) =
                    crate::db::models::set_video_lyrics_reference(&self.pool, video_id, true).await
                {
                    warn!(youtube_id = %youtube_id, %e, "reference_stage: failed to set lyrics_reference=1");
                }
                let aligned = crate::lyrics::backend::AlignedTrack {
                    lines,
                    provenance: format!("{}+mtl@rev1/g35t-ok", best.source),
                    raw_confidence: 1.0,
                };
                Some(align_track_to_lyrics_track(
                    aligned,
                    LYRICS_PIPELINE_VERSION,
                ))
            }
            crate::lyrics::orchestrator::ReferenceStageResult::Fail {
                reason,
                stats,
                mtl_device,
                mtl_elapsed_s,
                asr_word_count,
            } => {
                let reason_str = gate_fail_reason_str(&reason);
                warn!(
                    youtube_id = %youtube_id,
                    reason = reason_str,
                    matched_frac = stats.matched_frac,
                    "reference_stage: gate FAIL — keeping existing route (#143)"
                );
                crate::lyrics::audit_ctx::write_alignment_audit(
                    Some(&audit_ctx),
                    &reference_gate_audit_json(
                        "fail",
                        Some(reason_str),
                        Some(&stats),
                        Some(&mtl_device),
                        Some(mtl_elapsed_s),
                        asr_word_count,
                    ),
                )
                .await;
                let _ = crate::db::models::set_video_lyrics_reference(&self.pool, video_id, false)
                    .await;
                None
            }
            crate::lyrics::orchestrator::ReferenceStageResult::Error { stage, message } => {
                warn!(
                    youtube_id = %youtube_id,
                    stage,
                    %message,
                    "reference_stage: error — keeping existing route (#143)"
                );
                let reason = format!("{stage}: {message}");
                crate::lyrics::audit_ctx::write_alignment_audit(
                    Some(&audit_ctx),
                    &reference_gate_audit_json("error", Some(&reason), None, None, None, 0),
                )
                .await;
                let _ = crate::db::models::set_video_lyrics_reference(&self.pool, video_id, false)
                    .await;
                None
            }
        }
    }

    /// #154: raw `lyrics_gpu_mem_fraction` DB setting (the operator-tunable
    /// VRAM cap for the GPU workers; clamped later by
    /// `gpu_policy::env_for_child`). `None` when unset → the child uses the
    /// default cap. Defined here to keep `worker.rs` under the 1000-line cap.
    #[cfg_attr(test, mutants::skip)]
    pub(crate) async fn gpu_mem_setting(&self) -> Option<String> {
        crate::db::models::get_setting(&self.pool, "lyrics_gpu_mem_fraction")
            .await
            .ok()
            .flatten()
    }
}

/// Chooses the `lyrics_alignment_model` literal from a persisted
/// `LyricsTrack.source` label. Precedence:
///   - source label contains `mtl@rev1` (Lever-2 reference stage, #143) →
///     ALIGNMENT_MODEL_MTL_REV1 — checked FIRST since the stamped label is
///     `"<candidate.source>+mtl@rev1/g35t-ok"`, which could otherwise
///     collide with a `timed-merge`-labelled candidate source.
///   - source label contains `whisperx` → WHISPERX_V3_REV1
///   - source label contains `timed-merge` → TIMED_MERGE
///   - source label is exactly `yt_subs` / `lrclib` / `spotify` (raw
///     ship-through, no alignment ran) → NONE
///   - anything else → None (NULL — unknown model, e.g. legacy
///     ensemble:gemini paths that may still appear in `track.source`)
pub(crate) fn alignment_model_for_source(source: &str) -> Option<&'static str> {
    if source.contains("mtl@rev1") {
        Some(crate::lyrics::ALIGNMENT_MODEL_MTL_REV1)
    } else if source.contains("whisperx") {
        Some(crate::lyrics::ALIGNMENT_MODEL_WHISPERX_V3_REV1)
    } else if source.contains("timed-merge") {
        Some(crate::lyrics::ALIGNMENT_MODEL_TIMED_MERGE)
    } else if source == "yt_subs" || source == "lrclib" || source == "spotify" {
        Some(crate::lyrics::ALIGNMENT_MODEL_NONE)
    } else {
        None
    }
}

/// String literal for a `reference_gate::GateFailReason` — matched by value
/// rather than relying on a `Debug` derive on the (Part A-owned) enum.
fn gate_fail_reason_str(reason: &crate::lyrics::reference_gate::GateFailReason) -> &'static str {
    use crate::lyrics::reference_gate::GateFailReason;
    match reason {
        GateFailReason::Coverage => "coverage",
        GateFailReason::Offset => "offset",
        GateFailReason::Agreement => "agreement",
    }
}

/// Builds the `{youtube_id}_alignment_audit.json` payload for a Lever-2
/// (#143) reference-stage `Fail` (`stats = Some(..)`) or `Error`
/// (`stats = None`) outcome.
fn reference_gate_audit_json(
    verdict: &str,
    reason: Option<&str>,
    stats: Option<&crate::lyrics::reference_gate::GateStats>,
    mtl_device: Option<&str>,
    mtl_elapsed_s: Option<f64>,
    asr_words: usize,
) -> serde_json::Value {
    serde_json::json!({
        "verdict": verdict,
        "reason": reason,
        "lines_total": stats.map(|s| s.lines_total).unwrap_or(0),
        "lines_matched": stats.map(|s| s.lines_matched).unwrap_or(0),
        "matched_frac": stats.map(|s| s.matched_frac).unwrap_or(0.0),
        "median_signed_ms": stats.map(|s| s.median_signed_ms).unwrap_or(0),
        "within_400_frac": stats.map(|s| s.within_400_frac).unwrap_or(0.0),
        "mtl_device": mtl_device,
        "mtl_elapsed_s": mtl_elapsed_s,
        "asr_words": asr_words,
    })
}
