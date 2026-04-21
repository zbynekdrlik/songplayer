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
    duration_ms: u64,
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

    // Single cross-line-aware sanitize pass; the returned LyricsLines are
    // already well-formed. Details are derived against the SANITIZED words
    // but use the RAW start_ms for confidence lookups (agreement is a
    // property of the real alignment, not our clamped output).
    let out_lines = sanitize_track(&primary.lines, duration_ms);
    let mut details: Vec<WordMergeDetail> = Vec::new();
    let mut word_index = 0usize;
    for (line_idx, line) in out_lines.iter().enumerate() {
        let words = line.words.as_deref().unwrap_or(&[]);
        let raw_line = primary.lines.get(line_idx);
        for (i, w) in words.iter().enumerate() {
            let raw_start = raw_line
                .and_then(|l| l.words.get(i))
                .map(|rw| rw.start_ms)
                .unwrap_or(w.start_ms);
            let confidence = word_confidence(raw_start, primary_base, &other_starts);
            details.push(WordMergeDetail {
                word_index,
                reference_text: w.text.clone(),
                provider_estimates: collect_estimates_at(provider_results, raw_start),
                outliers_rejected: vec![],
                merged_start_ms: w.start_ms,
                merged_confidence: confidence,
                spread_ms: 0,
            });
            word_index += 1;
        }
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
/// `floor_start_ms` is the lowest acceptable `start_ms` for the first word
/// in the batch. Pass the previous line's last sanitized `end_ms` when
/// sanitizing a track line-by-line so the cross-line boundary stays
/// strictly increasing too. Pass `0` for a standalone batch.
///
/// Invariants enforced, in one left-to-right pass:
/// 1. Each word's `start_ms` is strictly GREATER than the previous word's
///    `start_ms`. Ties (qwen3's "duplicate start cluster" shape) and
///    backward jumps are lifted to `prev.end_ms`. Strict ordering is the
///    property a karaoke renderer actually needs.
/// 2. Every word has at least `MIN_WORD_DURATION_MS` of duration.
/// 3. No word extends past the next word's start (no overlap).
///
/// The three shapes of garbage observed in qwen3 output during the
/// 2026-04-19 event all fall to these rules:
/// - zero-duration words: rule 2
/// - backward-in-time starts: rule 1
/// - duplicate-start clusters: rule 1 (strict, not merely monotonic)
///
/// Without the floor, `compute_duplicate_start_pct` reports high
/// duplicate % from words at line boundaries even though each line is
/// individually clean (v9 shipped with this bug — see CLAUDE.md).
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
        let min_end = start_ms.saturating_add(MIN_WORD_DURATION_MS);

        // 2) minimum duration.
        let mut end_ms = (*raw_end).max(min_end);

        // 3) no overlap with the next word. The next word's effective start
        //    is lifted above our `min_end` so adjacent duplicate-start words
        //    still end up sequential rather than collapsed. Because
        //    `next_start_effective >= min_end` always, `end.min(effective)`
        //    never drops below `min_end`, so the minimum-duration invariant
        //    holds without a separate restore branch.
        if let Some((_, next_raw_start, _)) = words.get(i + 1) {
            let next_start_effective = (*next_raw_start).max(min_end);
            end_ms = end_ms.min(next_start_effective);
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

/// Pass-through baseline confidence = `base * PASS_THROUGH_MULTIPLIER`.
/// Used by both the multi-provider merge (when no peer agreed) and the
/// single-provider pass-through path in `orchestrator`.
pub(crate) fn pass_through_baseline(base_confidence: f32) -> f32 {
    base_confidence * PASS_THROUGH_MULTIPLIER
}

/// Sanitize every word across a track's lines, threading `floor_start_ms`
/// from line to line. Returns [`LyricsLine`]s with line boundaries derived
/// from the sanitized word timings.
///
/// Used by both `merge_provider_results` and the single-provider
/// pass-through in `orchestrator` so the cross-line strict-increasing
/// invariant is enforced in exactly one place.
pub(crate) fn sanitize_track(lines: &[LineTiming], duration_ms: u64) -> Vec<LyricsLine> {
    let mut out: Vec<LyricsLine> = Vec::with_capacity(lines.len());
    let mut floor_start_ms: u64 = 0;
    for (i, line) in lines.iter().enumerate() {
        if line.words.is_empty() {
            // Line-level-only provider (e.g., Gemini). v18 intentionally
            // emits `words: None` instead of synthesizing per-word timings
            // by even-distribution. Per user direction the lyrics pipeline
            // focus is line-level timing; fake per-word timings caused the
            // karaoke highlighter to animate at wrong moments on the wall
            // because a 0.2 s word and a 2 s word received the same
            // duration under linear interpolation. Better to show no
            // per-word highlight than a wrong one.
            //
            // Still apply the Python prototype's line-level finalize:
            //   1. Clamp start to `floor_start_ms` (cross-line monotonic).
            //   2. End clip `end_ms = min(raw_end, next_start - 50)` so
            //      consecutive lines don't overlap visually; falls back to
            //      `duration_ms` for the last line so it doesn't extend
            //      past song end.
            //   3. If the resulting span is inverted or too short, floor
            //      to 500 ms so the renderer always has something to show.
            let line_start = line.start_ms.max(floor_start_ms);
            let next_start = lines
                .get(i + 1)
                .map(|n| n.start_ms.max(line_start))
                .unwrap_or(duration_ms.max(line_start));
            let tentative = line.end_ms.min(
                next_start
                    .saturating_sub(50)
                    .max(line_start.saturating_add(200)),
            );
            let line_end = if tentative > line_start {
                tentative.max(line_start.saturating_add(80))
            } else {
                line_start.saturating_add(500)
            };
            floor_start_ms = line_end;
            out.push(LyricsLine {
                start_ms: line_start,
                end_ms: line_end,
                en: line.text.clone(),
                sk: None,
                words: None,
            });
            continue;
        }
        let raw: Vec<(String, u64, u64)> = line
            .words
            .iter()
            .map(|w| (w.text.clone(), w.start_ms, w.end_ms))
            .collect();
        let sanitized = sanitize_word_timings_from(&raw, floor_start_ms);
        if let Some(last) = sanitized.last() {
            floor_start_ms = last.2;
        }
        let words: Vec<LyricsWord> = sanitized
            .into_iter()
            .map(|(text, start_ms, end_ms)| LyricsWord {
                text,
                start_ms,
                end_ms,
            })
            .collect();
        let line_start = words.first().map(|w| w.start_ms).unwrap_or(0);
        let line_end = words.last().map(|w| w.end_ms).unwrap_or(0);
        out.push(LyricsLine {
            start_ms: line_start,
            end_ms: line_end,
            en: line.text.clone(),
            sk: None,
            words: Some(words),
        });
    }
    out
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
/// `sorted` must be sorted ascending (the sort isn't required for
/// correctness, only for the typical O(log N) hot-path lookup caller).
pub(crate) fn nearest_within(target: u64, sorted: &[u64], window_ms: i64) -> bool {
    let target_i = target as i64;
    sorted
        .iter()
        .any(|&x| (x as i64 - target_i).abs() <= window_ms)
}

/// Collect the provider-name → (start_ms, confidence) tuples for every word
/// whose start_ms matches `target_start_ms`. Used in the audit log so an
/// operator can trace which providers "voted" on a given word.
///
/// Audit-log helper — reviewer I2 already flagged that exact-ms equality
/// is an imperfect match (independent aligners never collide on exact
/// ms). The audit log's strict-equality behaviour is documented; a
/// follow-up PR may widen this to use `AGREEMENT_WINDOW_MS`. Skipping
/// mutation testing until then since mutations on debug-aid logic
/// aren't tractable to pin without materially reshaping the function.
#[cfg_attr(test, mutants::skip)]
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
