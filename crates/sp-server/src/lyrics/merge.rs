//! Deterministic merge layer for ensemble alignment.
//!
//! Takes 1–N `ProviderResult`s (from qwen3 forced-aligner, autosub ASR,
//! etc.) and produces a single `LyricsTrack` using pure Rust math — no
//! LLM call. Previous design used Claude to reconcile timings, but LLMs
//! cannot reliably emit exact-length arrays: the word-count sanity check
//! failed on ~40% of production songs because Claude's tokenization of
//! contractions drifted from `split_whitespace`. The weighting rules are
//! deterministic (base_confidence * 0.7 pass-through, 1.2x boost on
//! cross-provider agreement) and run in microseconds here.
//!
//! The first function argument stays `&AiClient` for call-site compatibility
//! with orchestrator but is unused; a later refactor will drop it.

use anyhow::Result;
use sp_core::lyrics::{LyricsLine, LyricsTrack, LyricsWord};
use std::path::Path;
use tokio::fs;
use tracing::debug;

use crate::ai::client::AiClient;
use crate::lyrics::provider::*;

/// Distance in ms within which two provider timestamps count as agreeing
/// on the same word. Derived from legacy Claude prompt (rule 5).
const AGREEMENT_WINDOW_MS: i64 = 500;

/// Multiplier applied to base_confidence when at least one non-primary
/// provider has a word timestamp within `AGREEMENT_WINDOW_MS` of the
/// primary's. Capped at 1.0. Matches legacy prompt rule 5.
const AGREEMENT_BOOST: f32 = 1.2;

/// Multiplier applied to base_confidence when NO other provider agrees
/// (pass-through baseline). Matches legacy prompt rule 6.
const PASS_THROUGH_MULTIPLIER: f32 = 0.7;

/// Merge provider alignment results into a single `LyricsTrack`.
///
/// Deterministic algorithm:
/// 1. Pick the provider with the highest `base_confidence` that has at
///    least one word as PRIMARY. This is qwen3 in production (its forced
///    aligner emits per-reference-word timings).
/// 2. For each primary word at time T, check non-primary providers for a
///    word timestamp within `AGREEMENT_WINDOW_MS` of T.
///    - At least one agreement → confidence = min(1.0, base * 1.2).
///    - No agreement → confidence = base * 0.7 (pass-through baseline).
/// 3. Emit `LyricsTrack` tagged `ensemble:<p1>+<p2>+...` listing every
///    participating provider (even non-primary ones) so the audit log
///    and DB `lyrics_source` column show the ensemble composition.
///
/// The `_ai_client`, `_reference_text`, and `_reference_source` parameters
/// are kept for API compatibility with the orchestrator call site; they
/// are unused by this deterministic implementation.
#[allow(clippy::too_many_arguments)]
pub async fn merge_provider_results(
    _ai_client: &AiClient,
    _reference_text: &str,
    _reference_source: &str,
    provider_results: &[ProviderResult],
) -> Result<(LyricsTrack, Vec<WordMergeDetail>)> {
    let primary = pick_best_provider_with_words(provider_results)
        .ok_or_else(|| anyhow::anyhow!("no provider has usable word timings"))?;

    let primary_base = base_confidence_of(primary);

    // Collect every non-primary word's start timestamp into a flat sorted
    // vec for O(log N) nearest-neighbor agreement checks.
    let mut other_starts: Vec<u64> = provider_results
        .iter()
        .filter(|pr| !std::ptr::eq(*pr, primary))
        .flat_map(|pr| pr.lines.iter())
        .flat_map(|l| l.words.iter().map(|w| w.start_ms))
        .collect();
    other_starts.sort_unstable();

    debug!(
        "merge: primary={}, base={}, other_word_count={}",
        primary.provider_name,
        primary_base,
        other_starts.len()
    );

    let mut out_lines: Vec<LyricsLine> = Vec::with_capacity(primary.lines.len());
    let mut details: Vec<WordMergeDetail> = Vec::new();
    let mut word_index = 0usize;

    for primary_line in &primary.lines {
        let mut words: Vec<LyricsWord> = Vec::with_capacity(primary_line.words.len());
        for w in &primary_line.words {
            let confidence = word_confidence(w.start_ms, primary_base, &other_starts);
            details.push(WordMergeDetail {
                word_index,
                reference_text: w.text.clone(),
                provider_estimates: collect_estimates_at(provider_results, w.start_ms),
                outliers_rejected: vec![],
                merged_start_ms: w.start_ms,
                merged_confidence: confidence,
                spread_ms: 0,
            });
            word_index += 1;
            words.push(LyricsWord {
                text: w.text.clone(),
                start_ms: w.start_ms,
                end_ms: w.end_ms,
            });
        }
        let line_start = words.first().map(|w| w.start_ms).unwrap_or(0);
        let line_end = words.last().map(|w| w.end_ms).unwrap_or(0);
        out_lines.push(LyricsLine {
            start_ms: line_start,
            end_ms: line_end,
            en: primary_line.text.clone(),
            sk: None,
            words: Some(words),
        });
    }

    let track = LyricsTrack {
        version: 2,
        source: format!(
            "ensemble:{}",
            provider_results
                .iter()
                .map(|p| p.provider_name.as_str())
                .collect::<Vec<_>>()
                .join("+")
        ),
        language_source: "en".into(),
        language_translation: String::new(),
        lines: out_lines,
    };

    Ok((track, details))
}

/// Pick the provider with the highest `base_confidence` that has at least one
/// line with at least one word.
pub(crate) fn pick_best_provider_with_words(
    provider_results: &[ProviderResult],
) -> Option<&ProviderResult> {
    provider_results
        .iter()
        .filter(|pr| pr.lines.iter().any(|l| !l.words.is_empty()))
        .max_by(|a, b| {
            let bc_a = base_confidence_of(a);
            let bc_b = base_confidence_of(b);
            bc_a.partial_cmp(&bc_b).unwrap_or(std::cmp::Ordering::Equal)
        })
}

/// Read the `base_confidence` metadata key, defaulting to 0.7 (qwen3 default).
pub(crate) fn base_confidence_of(pr: &ProviderResult) -> f32 {
    pr.metadata
        .get("base_confidence")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.7) as f32
}

/// Compute the per-word confidence: boost when any non-primary provider
/// has a timestamp within `AGREEMENT_WINDOW_MS`, otherwise pass-through.
///
/// Pure function — takes a pre-sorted slice of other-provider start_ms
/// timestamps for O(log N) binary-search lookup.
pub(crate) fn word_confidence(start_ms: u64, primary_base: f32, other_starts: &[u64]) -> f32 {
    if other_starts.is_empty() {
        // No peers to corroborate — pass-through.
        return primary_base * PASS_THROUGH_MULTIPLIER;
    }
    if nearest_within(start_ms, other_starts, AGREEMENT_WINDOW_MS) {
        (primary_base * AGREEMENT_BOOST).min(1.0)
    } else {
        primary_base * PASS_THROUGH_MULTIPLIER
    }
}

/// Returns true iff some element of `sorted` is within `window_ms` of `target`.
/// `sorted` must be sorted ascending.
pub(crate) fn nearest_within(target: u64, sorted: &[u64], window_ms: i64) -> bool {
    if sorted.is_empty() {
        return false;
    }
    let idx = sorted.partition_point(|&x| x < target);
    // Candidate(s) are sorted[idx] (first >= target) and sorted[idx-1] (last < target).
    let mut best: i64 = i64::MAX;
    if idx < sorted.len() {
        best = best.min((sorted[idx] as i64 - target as i64).abs());
    }
    if idx > 0 {
        best = best.min((target as i64 - sorted[idx - 1] as i64).abs());
    }
    best <= window_ms
}

/// Collect the provider-name → (start_ms, confidence) tuples for every word
/// whose start_ms matches `target_start_ms`. Used in the audit log so an
/// operator can trace which providers "voted" on a given word.
fn collect_estimates_at(
    provider_results: &[ProviderResult],
    target_start_ms: u64,
) -> Vec<(String, u64, f32)> {
    provider_results
        .iter()
        .filter_map(|pr| {
            pr.lines
                .iter()
                .flat_map(|l| &l.words)
                .find(|w| w.start_ms == target_start_ms)
                .map(|w| (pr.provider_name.clone(), w.start_ms, w.confidence))
        })
        .collect()
}

/// Write the audit log to disk alongside the lyrics JSON.
#[cfg_attr(test, mutants::skip)]
pub async fn write_audit_log(cache_dir: &Path, log: &AuditLog) -> Result<()> {
    let path = cache_dir.join(format!("{}_alignment_audit.json", log.video_id));
    let json = serde_json::to_string_pretty(log)?;
    fs::write(&path, json).await?;
    debug!("wrote audit log to {}", path.display());
    Ok(())
}

#[cfg(test)]
#[path = "merge_tests.rs"]
mod tests;
