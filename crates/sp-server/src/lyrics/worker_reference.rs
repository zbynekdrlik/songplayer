//! `LyricsWorker` extension — Lever-2 (#143) forced-alignment reference stage
//! (the v22 ★ tier).
//!
//! Extracted from `worker.rs::process_song` to keep that file under the
//! 1000-line CI limit. `run_mtl_reference_stage` is the FIRST tier: it aligns
//! the best text candidate via mtl, verifies it against the song's one Gemini
//! ASR transcript through the two-way reference gate
//! (`orchestrator::run_reference_stage`), and on gate PASS ships
//! the mtl line timings directly, under a `…/g35t-ok` source (#144 F1: the
//! persist sets `videos.lyrics_reference` from it, on every row of the video).
//! Every PASS/FAIL/ERROR writes the `{youtube_id}_alignment_audit.json`
//! sidecar (#144: a PASS too, so the gate's numbers — the sung coverage
//! included — are on disk for every ★ row, and a stale FAIL audit of an
//! earlier run never outlives a later PASS). FAIL/ERROR returns `None` so the
//! caller falls through to the v22 g35t base tier (`worker_g35t`).

use std::path::Path;

use sp_core::lyrics::LyricsTrack;
use tracing::{info, warn};

use super::worker::{LyricsWorker, align_track_to_lyrics_track};
use crate::lyrics::{LYRICS_PIPELINE_VERSION, REFERENCE_SOURCE_SUFFIX};

impl LyricsWorker {
    /// Lever 2 (#143): forced-alignment reference stage. See
    /// `orchestrator::run_reference_stage` for the mtl-align → gate decision
    /// against `words`, the song's one g35t transcript (#144); this wraps it
    /// with the skip conditions (an empty transcript is one: no gate can pass
    /// on it, so no mtl is spent) and the
    /// `_alignment_audit.json` sidecar of every gate outcome. `backend` is
    /// the injection seam (`orchestrator::ReferenceStageBackend`) —
    /// production passes `RealReferenceStageBackend`, tests pass a fake.
    ///
    /// Returns `Ok(Some(LyricsTrack))` on gate PASS — the caller ships it
    /// directly (★). Returns `Ok(None)` on skip/FAIL/ERROR — the caller falls
    /// through to the v22 g35t base tier (`worker_g35t`). Returns
    /// `Err(HeavyDefer)` (#161/#162) when the whole song must defer with NO
    /// backoff: `WallAbort` (the mtl subprocess was killed mid-align because the
    /// wall became busy → WaitingForWall) or `Memory` (free RAM/commit below the
    /// floor before the mtl spawn → WaitingForMemory). Either way the caller
    /// NEVER degrades to the base tier — the next pick re-runs mtl to identical ★.
    pub(crate) async fn run_mtl_reference_stage(
        &self,
        youtube_id: &str,
        best: Option<&crate::lyrics::tier1::CandidateText>,
        clean_vocal: Option<&Path>,
        words: &[crate::lyrics::g35t_client::AsrWord],
        backend: &dyn crate::lyrics::orchestrator::ReferenceStageBackend,
    ) -> Result<Option<LyricsTrack>, crate::lyrics::heavy_plan::HeavyDefer> {
        const MIN_LINES: usize = 4;

        // #144: the audit describes THIS pass — a stale one of an earlier pass
        // (a PASS audit included) goes, and a skip below leaves none.
        crate::lyrics::audit_ctx::remove_alignment_audit(&self.cache_dir, youtube_id).await;

        let mtl_cfg = crate::lyrics::mtl_aligner::MtlConfig::from_tools_dir(&self.tools_dir);
        if !mtl_cfg.is_available() {
            info!(
                youtube_id = %youtube_id,
                "reference_stage: mtl tooling unavailable — skipping (#143)"
            );
            return Ok(None);
        }
        let Some(wav) = clean_vocal else {
            info!(youtube_id = %youtube_id, "reference_stage: no vocals wav — skipping");
            return Ok(None);
        };
        let Some(best) = best else {
            info!(youtube_id = %youtube_id, "reference_stage: no text candidate — skipping");
            return Ok(None);
        };
        if best.lines.len() < MIN_LINES {
            info!(
                youtube_id = %youtube_id,
                lines = best.lines.len(),
                "reference_stage: candidate below the {MIN_LINES}-line floor — skipping"
            );
            return Ok(None);
        }
        if words.is_empty() {
            info!(youtube_id = %youtube_id, "reference_stage: empty transcript — skipping (#144)");
            return Ok(None);
        }
        // #144 F3: the gate's Coverage verdict reads only the text, and mtl
        // returns every line with its text unchanged, so a text that fails it
        // is failed here, before mtl (`coverage_fail_before_timing`): no
        // heavy slot, no mtl minutes. Such a text is often written once and
        // sung many times, which upstream's DP loops on (its first phone can
        // step from column -1, the last): its backtrack raised `IndexError`
        // on 5 SNV songs.
        if let Some(stats) =
            crate::lyrics::reference_gate::coverage_fail_before_timing(&best.lines, words)
        {
            warn!(
                youtube_id = %youtube_id,
                reason = "coverage",
                matched_frac = stats.matched_frac,
                sung_covered_frac = stats.sung_covered_frac,
                max_uncovered_sung_ms = stats.max_uncovered_sung_ms,
                "reference_stage: gate FAIL before mtl — Coverage (#144 F3: the fields tell which)"
            );
            let audit_ctx = crate::lyrics::audit_ctx::AuditContext {
                cache_dir: &self.cache_dir,
                youtube_id,
            };
            let mut audit = reference_gate_audit_json(
                "fail",
                Some(gate_fail_reason_str(
                    &crate::lyrics::reference_gate::GateFailReason::Coverage,
                )),
                Some(&stats),
                None,
                None,
                words.len(),
            );
            audit["before_mtl"] = serde_json::Value::Bool(true);
            // No line was timed: no offset and no agreement, never a "0 ms".
            audit["median_signed_ms"] = serde_json::Value::Null;
            audit["within_400_frac"] = serde_json::Value::Null;
            crate::lyrics::audit_ctx::write_alignment_audit(Some(&audit_ctx), &audit).await;
            return Ok(None);
        }

        // #167: no heavy step for the first 60 s after engine start — the wall
        // pipelines must come up on a quiet box. No backoff; re-picked next tick.
        if let Some(reg) = self.ndi_health_registry.as_ref()
            && crate::lyrics::idle_gate::startup_floor_defers(reg.since_created())
        {
            info!("lyrics_worker: heavy step mtl align deferred (wall unknown — startup grace)");
            return Err(crate::lyrics::heavy_plan::HeavyDefer::StartupGrace);
        }

        // #144 r2: QUEUE for the heavy slot (fair FIFO — block behind a running
        // child), then measure headroom AT SPAWN with the permit held (after the
        // skip conditions, so it only queues when mtl WILL run). Below the 4 GiB
        // floor → release the permit and defer the whole song with no backoff
        // (`WaitingForMemory`); the WARN fires inside `memory_ok_for`. Held across
        // `run_reference_stage` (incl. a CUDA-OOM `--no-cuda` retry), so the deep
        // acquire in `mtl_aligner::run_mtl_align` is gone — a second acquire on
        // the same task would deadlock the Semaphore(1).
        let _slot = match crate::lyrics::heavy_slot::acquire_slot_for_spawn("mtl align").await {
            Ok(g) => g,
            Err(_) => return Err(crate::lyrics::heavy_plan::HeavyDefer::Memory),
        };

        let outcome = crate::lyrics::orchestrator::run_reference_stage(
            backend,
            wav,
            youtube_id,
            &best.lines,
            words,
        )
        .await;

        let audit_ctx = crate::lyrics::audit_ctx::AuditContext {
            cache_dir: &self.cache_dir,
            youtube_id,
        };

        match outcome {
            crate::lyrics::orchestrator::ReferenceStageResult::Pass {
                lines,
                stats,
                mtl_device,
                mtl_elapsed_s,
                asr_word_count,
            } => {
                info!(
                    youtube_id = %youtube_id,
                    matched_frac = stats.matched_frac,
                    within_400_frac = stats.within_400_frac,
                    sung_covered_frac = stats.sung_covered_frac,
                    max_uncovered_sung_ms = stats.max_uncovered_sung_ms,
                    "reference_stage: gate PASS — the track is the ★ tier (#143; the persist writes ★)"
                );
                crate::lyrics::audit_ctx::write_alignment_audit(
                    Some(&audit_ctx),
                    &reference_gate_audit_json(
                        "pass",
                        None,
                        Some(&stats),
                        Some(&mtl_device),
                        Some(mtl_elapsed_s),
                        asr_word_count,
                    ),
                )
                .await;
                // #144 F1: no ★ here — the persist writes it WITH the track
                // (`REFERENCE_SOURCE_SUFFIX`), on every row of the video.
                let aligned = crate::lyrics::backend::AlignedTrack {
                    lines,
                    provenance: format!("{}+mtl@rev1{REFERENCE_SOURCE_SUFFIX}", best.source),
                    raw_confidence: 1.0,
                };
                Ok(Some(align_track_to_lyrics_track(
                    aligned,
                    LYRICS_PIPELINE_VERSION,
                )))
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
                    sung_covered_frac = stats.sung_covered_frac,
                    max_uncovered_sung_ms = stats.max_uncovered_sung_ms,
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
                Ok(None)
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
                Ok(None)
            }
            crate::lyrics::orchestrator::ReferenceStageResult::WallAborted { detail } => {
                // #161: mtl was killed because the wall became busy — this is
                // NOT an alignment failure, so no audit sidecar and
                // `lyrics_reference` is left untouched. Bubble the abort up so
                // the caller defers the whole song (WaitingForWall) and re-runs
                // mtl to identical output on the next idle pick.
                info!(
                    youtube_id = %youtube_id,
                    detail = %detail,
                    "reference_stage: mtl aborted — wall became busy (#161)"
                );
                Err(crate::lyrics::heavy_plan::HeavyDefer::WallAbort(
                    crate::lyrics::idle_gate_abort::WallAbort { detail },
                ))
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
///   - source label contains `mtl@rev1` (v21 reference stage, #143) →
///     ALIGNMENT_MODEL_MTL_REV1 — checked FIRST since the stamped label is
///     `"<candidate.source>+mtl@rev1/g35t-ok"`.
///   - source label is exactly the g35t base tier (#159) → G35T_REV1.
///   - source label is exactly `yt_subs` / `lrclib` / `spotify` (raw
///     ship-through, no alignment ran) → NONE (defensive; the v22 pipeline
///     force-aligns these via mtl rather than shipping raw).
///   - anything else → None (NULL — unknown model, e.g. legacy `ensemble:*`
///     / `whisperx` paths that may still appear on un-reprocessed DB rows).
pub(crate) fn alignment_model_for_source(source: &str) -> Option<&'static str> {
    if source.contains("mtl@rev1") {
        Some(crate::lyrics::ALIGNMENT_MODEL_MTL_REV1)
    } else if source == crate::lyrics::g35t_transcript::SOURCE_G35T {
        Some(crate::lyrics::ALIGNMENT_MODEL_G35T_REV1)
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
/// (#143) reference-stage `Pass` / `Fail` (`stats = Some(..)`) or `Error`
/// (`stats = None`) outcome. #144 adds the transcript → reference numbers
/// (`sung_words`, `sung_covered_frac`, `max_uncovered_sung_ms`) and
/// `sung_coverage_ok`, which tells a `coverage` failure of the sung direction
/// from one of the matched lines.
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
        "lines_timed": stats.map(|s| s.lines_timed).unwrap_or(0),
        "matched_frac": stats.map(|s| s.matched_frac).unwrap_or(0.0),
        "median_signed_ms": stats.map(|s| s.median_signed_ms).unwrap_or(0),
        "within_400_frac": stats.map(|s| s.within_400_frac).unwrap_or(0.0),
        "sung_words": stats.map(|s| s.sung_words).unwrap_or(0),
        "sung_covered_frac": stats.map(|s| s.sung_covered_frac).unwrap_or(0.0),
        "max_uncovered_sung_ms": stats.map(|s| s.max_uncovered_sung_ms).unwrap_or(0),
        "sung_coverage_ok": stats.map(|s| crate::lyrics::reference_gate::covers_what_is_sung(&s.sung())),
        "mtl_device": mtl_device,
        "mtl_elapsed_s": mtl_elapsed_s,
        "asr_words": asr_words,
        // #144 F3: true only for a Coverage FAIL decided before mtl ran.
        "before_mtl": false,
    })
}

#[cfg(test)]
#[path = "worker_reference_tests_mutants.rs"]
mod worker_reference_tests_mutants;
