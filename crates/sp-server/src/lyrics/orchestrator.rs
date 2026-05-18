//! Orchestrator — drives the tier chain for a single song.
//!
//! Flow: Tier-1 collect → branch on LineSynced/TextOnly/None →
//! WhisperX backend (Tier-2) when needed → text_reference_merge (TextOnly path) →
//! Returns `AlignedTrack`; the caller (worker) converts to `LyricsTrack` and
//! translates separately.
//!
//! The orchestrator does NOT hold fetcher factories. Instead,
//! `OrchestratorInput.candidates` carries the per-song pre-resolved
//! `Vec<CandidateText>` built by the worker from `candidate_texts`.
//! All I/O happens upstream in `gather_sources`; the orchestrator just
//! picks the best candidate via `tier1::pick_best`. `tier1::collect` +
//! `FetchFn` remain exported for any future fetcher that genuinely needs
//! orchestrator-time parallel I/O.
//!
//! Per `feedback_no_legacy_code.md`: this module imports NONE of
//! the legacy providers (gemini_provider, qwen3_provider,
//! autosub_provider, description_provider, text_merge).
//! Those are deleted in Phase G.

use std::path::Path;
use std::sync::Arc;

use thiserror::Error;
use tracing::info;

use crate::ai::client::AiClient;
use crate::lyrics::backend::{AlignOpts, AlignedTrack, AlignmentBackend, BackendError};
use crate::lyrics::claude_merge::best_authoritative_candidate;
use crate::lyrics::claude_merge::coverage_ok;
use crate::lyrics::line_splitter::{SplitConfig, split_track};
use crate::lyrics::text_reference_merge;
use crate::lyrics::tier1::{CandidateText, Tier1Result, pick_best};
use crate::lyrics::timed_reference_merge;

#[derive(Debug, Error)]
pub enum OrchestratorError {
    #[error("backend: {0}")]
    Backend(#[from] BackendError),
    #[error("no alignment available: {0}")]
    NoAlignment(String),
}

pub struct Orchestrator {
    pub backend: Arc<dyn AlignmentBackend>,
    pub ai_client: Arc<AiClient>,
    pub split_cfg: SplitConfig,
}

/// Per-song input to `Orchestrator::process`.
///
/// `candidates` is the pre-resolved candidate list built by the worker
/// from `gather_sources` (and any Spotify fetcher keyed on
/// `spotify_track_id`). All I/O has already happened in `gather_sources_impl`,
/// so the orchestrator passes them straight to `tier1::pick_best` without
/// re-wrapping in async closures. `tier1::collect` + `FetchFn` remain
/// exported for future fetchers that genuinely need per-song parallel I/O
/// at orchestrator time.
pub struct OrchestratorInput<'a> {
    /// Pre-resolved Tier-1 candidates for this song.
    pub candidates: Vec<CandidateText>,
    /// BCP-47 language code for the ASR backend (e.g. "en").
    pub language: &'a str,
    /// Path to the Mel-Roformer + anvuew dereverb vocal stem.
    /// `None` when `preprocess_vocals` failed or tooling is unavailable.
    /// If `None` and Tier-1 returns `TextOnly` or `None` (requiring backend
    /// alignment), `process` returns `OrchestratorError::NoAlignment`.
    /// Tier-1 `LineSynced` short-circuits before the backend is reached and
    /// therefore succeeds even when this is `None`.
    pub vocal_wav: Option<&'a Path>,
    /// Per-song debug-output sink. When populated, every alignment + merge
    /// stage writes a JSON sidecar to `cache_dir` for permanent visibility:
    /// `{youtube_id}_whisperx_track.json` (raw alignment backend output)
    /// and `{youtube_id}_descmerge_audit.json` (description-merge per-phase
    /// state). When `None`, sidecar writes are skipped — used by tests.
    pub audit: Option<crate::lyrics::audit_ctx::AuditContext<'a>>,
}

impl Orchestrator {
    pub fn new(
        backend: Arc<dyn AlignmentBackend>,
        ai_client: Arc<AiClient>,
        split_cfg: SplitConfig,
    ) -> Self {
        Self {
            backend,
            ai_client,
            split_cfg,
        }
    }

    /// Run the full tier chain for one song and return an `AlignedTrack`.
    ///
    /// The caller (worker) is responsible for:
    /// - Building `OrchestratorInput.candidates` from `candidate_texts`
    /// - Converting `AlignedTrack` → `LyricsTrack` after this returns
    /// - Calling the translator on the resulting `LyricsTrack`
    #[cfg_attr(test, mutants::skip)] // Async orchestration glue; full-tier-chain integration is exercised end-to-end on win-resolume reprocess. Mutants on the branch decisions (LineSynced/TextOnly/None, yt_subs detection, has_timing+coverage_ok routing) flip semantically-equivalent branches that all converge on the same `text_reference_merge` or `timed_reference_merge` calls already covered by their own unit tests.
    pub async fn process(
        &self,
        input: OrchestratorInput<'_>,
    ) -> Result<AlignedTrack, OrchestratorError> {
        // Step 1: Pick the best Tier-1 candidate from the pre-resolved list.
        let tier1_result = pick_best(input.candidates);

        // Step 2: Branch on Tier-1 outcome.
        match tier1_result {
            Tier1Result::LineSynced(aligned_lines) => {
                // yt_subs has authoritative line text but YouTube auto-caption
                // line breaks split mid-phrase. The description+whisperx
                // pipeline (text_reference_merge) already solves chorus
                // repeats (Phase 2 + 2.8 sliding-window LCS), Claude line
                // mapping (Phase 1 — far stronger than forward-greedy LCS),
                // mishearing absorbs (2.6/2.65/2.7), karaoke split (Phase 3
                // Claude + 4 emit_with_subs), and cap+monotonic (Phase 5).
                // Route yt_subs through that same pipeline by clustering
                // caption-window adjacent lines into phrases and treating
                // them as a text candidate (yt_subs internal timing is
                // discarded; whisperx provides word-level boundaries).
                //
                // spotify / lrclib still short-circuit through
                // timed_reference_merge Mode B — their line breaks already
                // match phrase boundaries.
                let is_yt_subs = aligned_lines.provenance == "yt_subs"
                    || aligned_lines.provenance.starts_with("tier1:yt_subs");
                if is_yt_subs {
                    // yt_subs is the AUTHORITY for what is sung. Trust
                    // its lines + per-line timing. Only re-break LONG
                    // phrase clusters into karaoke sub-lines, with
                    // whisperx providing internal sub-line anchors when
                    // available and proportional interpolation when
                    // whisperx missed/mistranscribed words. yt_subs
                    // text is never dropped or substituted.
                    info!(
                        provenance = %aligned_lines.provenance,
                        lines = aligned_lines.lines.len(),
                        "orchestrator: Tier-1 yt_subs LineSynced → per-caption-window Claude split + whisperx anchors with proportional fallback"
                    );
                    let wav_opt = input.vocal_wav;
                    let asr_opt: Option<AlignedTrack> = if let Some(wav) = wav_opt {
                        match self
                            .backend
                            .align(wav, input.language, &AlignOpts::default())
                            .await
                        {
                            Ok(a) => {
                                crate::lyrics::audit_ctx::write_whisperx_track(
                                    input.audit.as_ref(),
                                    &a,
                                )
                                .await;
                                Some(a)
                            }
                            Err(e) => {
                                tracing::warn!(
                                    %e,
                                    "orchestrator: yt_subs whisperx align failed; using proportional split only"
                                );
                                None
                            }
                        }
                    } else {
                        None
                    };
                    let asr_words: Vec<crate::lyrics::backend::AlignedWord> = asr_opt
                        .as_ref()
                        .map(|a| {
                            a.lines
                                .iter()
                                .filter_map(|l| l.words.as_ref())
                                .flatten()
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default();
                    // Process EACH yt_subs caption window individually.
                    // yt_subs's per-line start_ms / end_ms are authoritative
                    // and must NOT be merged with neighbors. Long lines
                    // (>32c) get Claude-split with sub[0] anchored at
                    // yt_subs.start, sub[N-1] at yt_subs.end, internal
                    // sub boundaries from whisperx (proportional fallback).
                    let mut output: Vec<crate::lyrics::backend::AlignedLine> =
                        Vec::with_capacity(aligned_lines.lines.len());
                    for line in &aligned_lines.lines {
                        let split = crate::lyrics::yt_subs_split::split_cluster(
                            &self.ai_client,
                            &line.text,
                            line.start_ms,
                            line.end_ms,
                            &asr_words,
                        )
                        .await;
                        output.extend(split);
                    }
                    let provenance = match asr_opt.as_ref() {
                        Some(a) => format!("yt_subs+{}", a.provenance),
                        None => "yt_subs+timed-merge".into(),
                    };
                    return Ok(AlignedTrack {
                        lines: output,
                        provenance,
                        raw_confidence: asr_opt.map(|a| a.raw_confidence).unwrap_or(1.0),
                    });
                }

                // spotify / lrclib short-circuit (no ASR needed).
                info!(
                    provenance = %aligned_lines.provenance,
                    lines = aligned_lines.lines.len(),
                    "orchestrator: Tier-1 short-circuit (line-synced), routing to timed_reference_merge Mode B"
                );
                let candidate = aligned_lines_to_candidate(&aligned_lines);
                let song_duration_ms = candidate
                    .line_timings
                    .as_ref()
                    .and_then(|t| t.last())
                    .map(|(_, e)| (*e) as u32)
                    .unwrap_or(0);
                match timed_reference_merge::process(
                    None,
                    None,
                    &candidate,
                    song_duration_ms,
                    input.audit.as_ref(),
                )
                .await
                {
                    Ok(track) => Ok(track),
                    Err(e) => {
                        let fallback_lines = aligned_lines.lines;
                        let fallback_prov = aligned_lines.provenance;
                        tracing::warn!(
                            provenance = %fallback_prov,
                            error = %e,
                            "orchestrator: timed_reference_merge failed on LineSynced — falling back to split_track"
                        );
                        let pre_split = AlignedTrack {
                            lines: fallback_lines,
                            provenance: fallback_prov,
                            raw_confidence: 1.0,
                        };
                        Ok(split_track(&pre_split, self.split_cfg))
                    }
                }
            }
            Tier1Result::TextOnly(text_candidates) => {
                // Text-only path: run WhisperX for word timing, pick best
                // authoritative candidate, then route through
                // text_reference_merge::process (the unified text-merge
                // pipeline). Per the 2026-05-07 unification spec the
                // single-Claude-call merge in claude_merge::merge is retired.
                //
                // Provenance shape: `{best.source}+{asr.provenance}` (e.g.
                // "description+whisperx-large-v3@rev1", "genius+whisperx-large-v3@rev1").
                //
                // text_reference_merge runs its own Claude line-split (Phase 3)
                // internally — no external split_track wrap is needed when it
                // succeeds. On failure, fall back to split_track on raw
                // WhisperX so the song still ships timed lyrics.
                let wav = input.vocal_wav.ok_or_else(|| {
                    OrchestratorError::NoAlignment(
                        "Tier-1 TextOnly path requires a vocal WAV but none was available \
                         (preprocess_vocals failed or tooling is absent)"
                            .into(),
                    )
                })?;
                let asr = self
                    .backend
                    .align(wav, input.language, &AlignOpts::default())
                    .await?;
                crate::lyrics::audit_ctx::write_whisperx_track(input.audit.as_ref(), &asr).await;

                let best = match best_authoritative_candidate(&text_candidates) {
                    Some(b) if !b.lines.is_empty() => b,
                    _ => {
                        info!(
                            provenance = %asr.provenance,
                            "orchestrator: TextOnly with no usable candidate — shipping raw WhisperX with line split"
                        );
                        return Ok(split_track(&asr, self.split_cfg));
                    }
                };

                let song_duration_ms = asr.lines.last().map(|l| l.end_ms).unwrap_or(0);

                if best.has_timing && coverage_ok(best, song_duration_ms) {
                    info!(
                        provenance = %asr.provenance,
                        best_source = %best.source,
                        song_duration_ms,
                        "orchestrator: Tier-1 TextOnly + timed candidate (coverage_ok) → timed_reference_merge Mode A"
                    );
                    match timed_reference_merge::process(
                        Some(self.ai_client.as_ref()),
                        Some(&asr),
                        best,
                        song_duration_ms,
                        input.audit.as_ref(),
                    )
                    .await
                    {
                        Ok(track) => return Ok(track),
                        Err(e) => {
                            tracing::warn!(
                                provenance = %asr.provenance,
                                best_source = %best.source,
                                error = %e,
                                "orchestrator: timed_reference_merge failed — retrying via text_reference_merge"
                            );
                            // fall through to the text_reference_merge branch below
                        }
                    }
                }

                info!(
                    provenance = %asr.provenance,
                    asr_lines = asr.lines.len(),
                    text_candidates = text_candidates.len(),
                    best_source = %best.source,
                    best_has_timing = best.has_timing,
                    "orchestrator: Tier-1 TextOnly — backend called, routing to text_reference_merge"
                );

                match text_reference_merge::process(
                    &self.ai_client,
                    &asr,
                    best,
                    input.audit.as_ref(),
                )
                .await
                {
                    Ok(merged) => Ok(merged),
                    Err(e) => {
                        tracing::warn!(
                            provenance = %asr.provenance,
                            best_source = %best.source,
                            error = %e,
                            "orchestrator: text_reference_merge failed — falling back to raw WhisperX with line split"
                        );
                        Ok(split_track(&asr, self.split_cfg))
                    }
                }
            }
            Tier1Result::None => {
                // No text candidates at all — run WhisperX and ship its output
                // with the line splitter (no reconciliation possible without reference text).
                let wav = input.vocal_wav.ok_or_else(|| {
                    OrchestratorError::NoAlignment(
                        "Tier-1 None path requires a vocal WAV but none was available \
                         (preprocess_vocals failed or tooling is absent)"
                            .into(),
                    )
                })?;
                let asr = self
                    .backend
                    .align(wav, input.language, &AlignOpts::default())
                    .await?;
                crate::lyrics::audit_ctx::write_whisperx_track(input.audit.as_ref(), &asr).await;
                info!(
                    provenance = %asr.provenance,
                    asr_lines = asr.lines.len(),
                    "orchestrator: Tier-1 None — backend called, no reconciliation"
                );
                Ok(split_track(&asr, self.split_cfg))
            }
        }
    }
}

/// Convert a `Tier1::LineSynced` payload into a timed `CandidateText` so
/// the orchestrator can route it through `timed_reference_merge::process`
/// (Mode B). Source is taken from the `AlignedLines.provenance` (which is
/// the original tier1 source label like `"tier1:spotify"`).
fn aligned_lines_to_candidate(
    aligned_lines: &crate::lyrics::tier1::AlignedLines,
) -> crate::lyrics::tier1::CandidateText {
    let lines: Vec<String> = aligned_lines.lines.iter().map(|l| l.text.clone()).collect();
    let line_timings: Vec<(u64, u64)> = aligned_lines
        .lines
        .iter()
        .map(|l| (l.start_ms as u64, l.end_ms as u64))
        .collect();
    crate::lyrics::tier1::CandidateText {
        source: aligned_lines.provenance.clone(),
        lines,
        line_timings: Some(line_timings),
        has_timing: true,
    }
}

/// Decide whether the gathered text candidates contain at least one source
/// from the allowed set. Called by the worker after `gather_sources` and
/// before any expensive alignment dispatch (Demucs, whisperx, Claude-merge).
///
/// Allowed sources:
/// - `yt_subs`, `lrclib`, `spotify` — accepted when `has_timing == true`
///   (line-timed text already; whisperx only assists with long-line splits)
/// - `description` — accepted when `lines.is_empty() == false`
///   (curated text; whisperx performs full alignment against it)
///
/// Anything else (`genius`, `lrclib` without timing, raw whisperx with no
/// text reference, empty candidate list) is rejected. The worker stamps the
/// row with `lyrics_source = 'unsupported_source'` and bails — see
/// `db::models::mark_unsupported_source`.
///
/// See `docs/superpowers/specs/2026-05-16-lyrics-source-gating-design.md`.
pub(crate) fn is_allowed_text_source(
    candidates: &[crate::lyrics::provider::CandidateText],
) -> bool {
    candidates.iter().any(|c| match c.source.as_str() {
        "yt_subs" | "lrclib" | "spotify" => c.has_timing,
        "description" => !c.lines.is_empty(),
        _ => false,
    })
}

#[cfg(test)]
#[path = "orchestrator_gate_tests.rs"]
mod is_allowed_text_source_tests;

#[cfg(test)]
#[path = "orchestrator_tests.rs"]
mod tests;
