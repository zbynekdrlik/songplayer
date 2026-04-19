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
    let mut floor_start_ms: u64 = 0;

    for primary_line in &primary.lines {
        // Sanitize the primary provider's word timings before emitting.
        // Qwen3's forced aligner sometimes produces:
        //   - zero-duration words (start_ms == end_ms)
        //   - words that go backward in time (start_ms < previous.start_ms)
        //   - duplicate start_ms clusters
        //   - words that extend past the next word's start
        //   - words at line boundaries that share start_ms with the
        //     previous line's last word (cross-line duplicate that
        //     `compute_duplicate_start_pct` counts after sorting globally)
        // These propagate through untouched without sanitization and
        // manifest on stage as blinking / stuck / out-of-sync karaoke
        // subtitles (seen on SO BE IT during 2026-04-19 event).
        let raw_words: Vec<(String, u64, u64)> = primary_line
            .words
            .iter()
            .map(|w| (w.text.clone(), w.start_ms, w.end_ms))
            .collect();
        let sanitized = sanitize_word_timings_from(&raw_words, floor_start_ms);
        // Next line's first word must start strictly AFTER this line's
        // last sanitized end — otherwise the global dup check fires.
        if let Some(last) = sanitized.last() {
            floor_start_ms = last.2;
        }

        let mut words: Vec<LyricsWord> = Vec::with_capacity(sanitized.len());
        for (i, (text, start_ms, end_ms)) in sanitized.iter().enumerate() {
            // Prefer the confidence computed against the RAW start timestamp
            // (agreement with other providers is a property of the real
            // alignment, not of our sanitized boundaries).
            let raw_start = primary_line
                .words
                .get(i)
                .map(|w| w.start_ms)
                .unwrap_or(*start_ms);
            let confidence = word_confidence(raw_start, primary_base, &other_starts);
            details.push(WordMergeDetail {
                word_index,
                reference_text: text.clone(),
                provider_estimates: collect_estimates_at(provider_results, raw_start),
                outliers_rejected: vec![],
                merged_start_ms: *start_ms,
                merged_confidence: confidence,
                spread_ms: 0,
            });
            word_index += 1;
            words.push(LyricsWord {
                text: text.clone(),
                start_ms: *start_ms,
                end_ms: *end_ms,
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

/// Minimum per-word duration in ms. Zero-duration words from the forced
/// aligner are clamped to this — below ~50 ms the karaoke highlight
/// flickers faster than human perception can track.
const MIN_WORD_DURATION_MS: u64 = 80;

/// Sanitize a sequence of `(text, start_ms, end_ms)` tuples so the emitted
/// word list has well-formed timings for karaoke display. Pure function.
///
/// Invariants enforced, in one left-to-right pass:
///   1. Each word's `start_ms` is strictly GREATER than the previous word's
///      `start_ms`. Ties (qwen3's "duplicate start cluster" shape) and
///      backward jumps are lifted to `prev.end_ms`. Strict ordering is the
///      property a karaoke renderer actually needs — without it, multiple
///      words all "happen" at the same instant and the highlight cursor
///      can't resolve which word is active.
///   2. Every word has at least `MIN_WORD_DURATION_MS` of duration.
///   3. No word extends past the next word's start (no overlap).
///
/// The three shapes of garbage observed in qwen3 output during the
/// 2026-04-19 event all fall to these rules:
///   - zero-duration words: rule 2
///   - backward-in-time starts: rule 1
///   - duplicate-start clusters: rule 1 (strict, not merely monotonic)
pub(crate) fn sanitize_word_timings(words: &[(String, u64, u64)]) -> Vec<(String, u64, u64)> {
    sanitize_word_timings_from(words, 0)
}

/// Same as [`sanitize_word_timings`] but seeded with a `floor_start_ms`.
/// Use this when sanitizing line-by-line: pass the previous line's last
/// sanitized `end_ms` as the floor so the cross-line boundary stays
/// strictly increasing too. Otherwise `compute_duplicate_start_pct`
/// reports high duplicate % from words at line boundaries even though
/// each line is individually clean.
pub(crate) fn sanitize_word_timings_from(
    words: &[(String, u64, u64)],
    floor_start_ms: u64,
) -> Vec<(String, u64, u64)> {
    let mut out: Vec<(String, u64, u64)> = Vec::with_capacity(words.len());
    for (i, (text, raw_start, raw_end)) in words.iter().enumerate() {
        // 1) strict start: next word must start AFTER previous word ended.
        //    For the first word in the batch, `floor_start_ms` plays the
        //    role of prev_end (so a new line inherits the previous line's
        //    end as its lower bound).
        let prev_end = out.last().map(|w| w.2).unwrap_or(floor_start_ms);
        let start_ms = (*raw_start).max(prev_end);

        // 2) minimum duration.
        let mut end_ms = (*raw_end).max(start_ms.saturating_add(MIN_WORD_DURATION_MS));

        // 3) no overlap with the next word, if a next word exists.
        //    Peek at the NEXT raw start, but also lift it above our current
        //    `start_ms` so adjacent duplicate-start words still end up
        //    sequential rather than collapsing into a zero-duration range.
        if let Some((_, next_raw_start, _)) = words.get(i + 1) {
            let next_start_effective =
                (*next_raw_start).max(start_ms.saturating_add(MIN_WORD_DURATION_MS));
            if end_ms > next_start_effective {
                end_ms = next_start_effective;
            }
            // Preserve minimum duration if clamping made the word too short.
            if end_ms < start_ms.saturating_add(MIN_WORD_DURATION_MS) {
                end_ms = start_ms.saturating_add(MIN_WORD_DURATION_MS);
            }
        }

        out.push((text.clone(), start_ms, end_ms));
    }
    out
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
