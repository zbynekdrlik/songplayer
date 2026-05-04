//! Description / override merge pipeline (issue #78 full fix).
//!
//! Phases:
//! 1. Claude line-mapping (primary) or NW DP (fallback) — assign each
//!    asr_word to one ref line, monotonic line order.
//! 2. Chorus repeat re-emit for long unmatched audio gaps (worship songs
//!    repeat chorus 3-4× but description lists each unique line once).
//! 2.5. Trim trailing-outlier matched indices so derived span ≤ 8 s.
//! 3. Claude-driven natural-phrase splits for >32-char lines (LED wall cap).
//! 4. Emit AlignedLine list with sub-line word-level timing (second LCS
//!    within parent's matched word range).
//! 5. 8 s display cap, monotonic floor-clamp, drop micro-windows.
//!
//! Per `feedback_line_timing_only.md` every emitted `AlignedLine` ships
//! `words: None`. Per `feedback_no_even_distribution.md` no uniform-spacing
//! ever; all timings derive from real ASR word-timestamps.
//!
//! Provenance: `"{candidate.source}+{asr.provenance}"`. No `+claude-merge`
//! suffix; Claude only splits long lines, no semantic merging.

// Algorithm uses several index-based scans (LCS DP, gap detection, char-index
// split-point search) where the iter-chain rewrite obscures intent or pulls
// awkward zip(enumerate(...)) patterns. Allow indexed loops for this module.
#![allow(clippy::needless_range_loop)]

use std::collections::HashMap;

use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::ai::client::AiClient;
use crate::lyrics::backend::{AlignedLine, AlignedTrack};
use crate::lyrics::claude_merge::{MergeError, drop_hallucinated_lead_in};
use crate::lyrics::tier1::CandidateText;

#[path = "description_merge_mapping.rs"]
mod mapping;

#[path = "description_merge_audit.rs"]
mod audit;

/// Hard upper bound for sub-line EN length. The LED wall renders only this
/// many characters per row; longer lines visually overflow into adjacent UI
/// panels and are unacceptable.
pub const SUBLINE_MAX_CHARS: usize = 32;

/// Cap on a single line's display duration. When the LCS finds no ref-line
/// match for an extended window of audio (instrumental, vocal break, chorus
/// section without unique words), the previous matched line's `end_ms` would
/// stretch to fill the gap. Cap stops that — line displays for `LONG_LINE_CAP_MS`
/// then wall goes blank until the next matched line.
pub const LONG_LINE_CAP_MS: u32 = 8000;

/// Minimum gap between consecutive matched lines that triggers chorus-repeat
/// detection. Below this we trust the LCS gap as a real silence between sung
/// phrases; above this we look for a ref line that could fill it.
const CHORUS_REPEAT_GAP_MS: u32 = 4000;

/// Minimum word-level match score for a chorus repeat to be emitted (matched
/// ASR words / ref words ratio). Below threshold the gap stays blank.
const CHORUS_REPEAT_MIN_MATCH_RATIO: f32 = 0.6;

/// Minimum number of ASR words a chorus repeat must match. Without this floor
/// a 2-word ref like "Holy forever" with ratio 0.5 would emit on a single
/// "holy" audio word, producing a near-zero-duration display flash.
const CHORUS_REPEAT_MIN_MATCHED_WORDS: usize = 2;

/// Minimum display duration for ANY emitted line. Short matches (single
/// audio word, fragments) collapse to ~0 ms display windows that flash by
/// invisibly on the wall. We drop emits below this floor in Phase 5.
const MIN_LINE_DURATION_MS: u32 = 500;

/// One ref-line emission with its matched ASR word-stream indices. May be
/// either an original-pass match (Phase 1) or a chorus-repeat re-emission
/// (Phase 2).
#[derive(Clone, Debug)]
struct LineEmit {
    text: String,
    /// Indices into the global flattened `asr_words` vec.
    asr_word_indices: Vec<usize>,
}

#[derive(Clone, Debug)]
struct AsrWord {
    norm: String,
    start_ms: u32,
    end_ms: u32,
}

/// Public entry: full description/override pipeline.
///
/// Returns an `AlignedTrack` whose every line has `words: None`, sub-line EN
/// length ≤ 32 chars, line duration ≤ 8 s, sub-line timings from real ASR
/// word ranges, and chorus repeats re-emitted to fill long unmatched gaps.
pub async fn process(
    ai_client: &AiClient,
    asr: &AlignedTrack,
    candidate: &CandidateText,
    audit_ctx: Option<&crate::lyrics::audit_ctx::AuditContext<'_>>,
) -> Result<AlignedTrack, MergeError> {
    let asr_words = flatten_asr(asr);
    if asr_words.is_empty() {
        // No usable ASR timing — fall back to ref text with placeholder timings.
        return Ok(emit_unmatched_only(asr, candidate));
    }

    let ref_lines: &[String] = &candidate.lines;
    if ref_lines.is_empty() {
        return Err(MergeError::NoReference);
    }

    let mut audit_state =
        audit::AuditState::new(ref_lines, &asr_words, &candidate.source, &asr.provenance);

    // Phase 1: Claude line-mapping (primary) with NW DP fallback. Claude
    // reads phrasing semantically; the deterministic DP is a guaranteed-correct
    // floor on parse / network / refusal failure. See description_merge_mapping.
    let (mut emits, phase1_provider) =
        match mapping::claude_map_words_to_lines(ai_client, ref_lines, &asr_words).await {
            Ok(map) => {
                info!(
                    ref_lines = ref_lines.len(),
                    asr_words = asr_words.len(),
                    "description_merge: claude line-mapping succeeded"
                );
                (mapping::emits_from_mapping(&map, ref_lines), "claude")
            }
            Err(e) => {
                warn!(
                    %e,
                    ref_lines = ref_lines.len(),
                    asr_words = asr_words.len(),
                    "description_merge: claude line-mapping failed; falling back to NW DP"
                );
                (match_ref_to_asr(ref_lines, &asr_words), "nw_dp")
            }
        };
    audit_state.record_phase1(phase1_provider, &emits, &asr_words);

    // Phase 2: chorus repeat re-emit for long unmatched gaps.
    let extras = detect_chorus_repeats(ref_lines, &asr_words, &emits);
    if !extras.is_empty() {
        info!(
            count = extras.len(),
            "description_merge: chorus repeat re-emit"
        );
    }
    emits.extend(extras);
    emits.sort_by_key(|e| match e.asr_word_indices.first() {
        Some(&i) => asr_words[i].start_ms,
        None => u32::MAX,
    });

    // Phase 2.5: trim trailing-outlier matched indices on every emit so its
    // derived audio span ≤ LONG_LINE_CAP_MS. See `trim_outlier_indices`.
    for e in emits.iter_mut() {
        trim_outlier_indices(&mut e.asr_word_indices, &asr_words);
    }

    audit_state.record_phase2(&emits, &asr_words);

    // Phase 3: Claude-driven natural-phrase splits for long lines (>32c).
    let needs_split: Vec<(usize, &str)> = emits
        .iter()
        .enumerate()
        .filter(|(_, e)| e.text.chars().count() > SUBLINE_MAX_CHARS)
        .map(|(i, e)| (i, e.text.as_str()))
        .collect();

    let split_map: HashMap<usize, Vec<String>> = if needs_split.is_empty() {
        HashMap::new()
    } else {
        match claude_split_lines(ai_client, &needs_split).await {
            Ok(map) => map,
            Err(e) => {
                warn!(
                    %e,
                    count = needs_split.len(),
                    "description_merge: claude split failed, falling back to deterministic word-boundary split"
                );
                deterministic_split_lines(&needs_split)
            }
        }
    };

    // Phase 4: emit AlignedLine list with sub-line word timings.
    let mut output: Vec<AlignedLine> = Vec::new();
    for (i, emit) in emits.iter().enumerate() {
        let subs = split_map.get(&i);
        let lines = aligned_lines_for_emit(emit, &asr_words, subs);
        output.extend(lines);
    }

    audit_state.record_pre_phase5(&output);

    // Phase 5: 8 s cap + monotonic enforcement.
    apply_cap_and_monotonic(&mut output);
    audit_state.record_post_phase5(&output);

    audit_state.write_to_disk(audit_ctx).await;

    Ok(AlignedTrack {
        lines: output,
        provenance: format!("{}+{}", candidate.source, asr.provenance),
        raw_confidence: asr.raw_confidence,
    })
}

// ── Phase 1: initial LCS match ────────────────────────────────────────────────

fn flatten_asr(asr: &AlignedTrack) -> Vec<AsrWord> {
    let mut out = Vec::new();
    for line in &asr.lines {
        let words = match &line.words {
            Some(w) if !w.is_empty() => w.clone(),
            _ => continue,
        };
        let words = drop_hallucinated_lead_in(words);
        for w in &words {
            let norm = normalize_word(&w.text);
            if !norm.is_empty() {
                out.push(AsrWord {
                    norm,
                    start_ms: w.start_ms,
                    end_ms: w.end_ms,
                });
            }
        }
    }
    out
}

/// Match reference lines to ASR audio words via global Needleman-Wunsch
/// alignment with line-aware grouping. Single principled algorithm — every
/// ref line uses the same DP, no per-line tweaks, no greedy windowing.
///
/// State: `dp[i][j]` = best alignment score after consuming `i` reference
/// words (across all lines, in order) and `j` audio words. Transitions:
///
/// - **Match**: `ref[i-1] == asr[j-1]` → `dp[i-1][j-1] + MATCH_BONUS`.
/// - **Skip ASR**: audio word is filler/mishearing/silence → `dp[i][j-1] +
///   SKIP_ASR_PENALTY`. Cheap so the algorithm freely emits silences /
///   instrumental passages between sung phrases.
/// - **Skip ref**: reference word missing in audio (singer drops a word) →
///   `dp[i-1][j] + SKIP_REF_PENALTY`. Expensive so we don't drop content.
///
/// Leading-edge boundary: `dp[0][j] = 0` lets the song open with arbitrary
/// silence/intro before the first ref word at zero cost. `dp[i][0]` decays
/// linearly so ref words can't be "matched" at audio_idx=0 for free.
///
/// After the DP forward pass, traceback collects each MATCH transition as a
/// `(ref_word_idx, asr_word_idx)` pair. Each ref word carries its line index
/// (from a flattened `ref_pairs` table built upfront), so grouping the matched
/// pairs by line index gives, for every ref line, the set of ASR words that
/// the alignment assigned to it. Min/max of those indices' word timestamps
/// becomes the line's display window.
///
/// Properties:
///
/// 1. *Globally optimal*. Different from local greedy: the score of every
///    transition contributes to the final picked path, so a sparse-but-early
///    match can lose to a denser-but-later match if the latter's gains
///    outweigh the SKIP_ASR cost of waiting.
/// 2. *Order-preserving*. Both axes only advance forward; ref lines come out
///    in their original order and each line's matched ASR indices are
///    contiguous-with-skips (no gaps shared with other ref lines).
/// 3. *Repetitive-content-tolerant*. Repeated chorus words in the audio
///    (e.g. "Holy" sung 12 times) don't all attach to the same ref line:
///    once the DP advances past a ref line (l → l+1), later "Holy" words
///    map to subsequent lines OR to silence (skip_asr) and Phase 2 picks
///    them up as chorus repeats.
/// 4. *Tunable trade-offs* via three constants below — explicit and
///    inspectable, not buried in heuristics.
fn match_ref_to_asr(ref_lines: &[String], asr_words: &[AsrWord]) -> Vec<LineEmit> {
    /// Reward for a true match. Anchors the scale.
    const MATCH_BONUS: f32 = 1.0;
    /// Cost of skipping an ASR word (silence / filler / mishear). Small so
    /// instrumental passages cost little — but non-zero so the path doesn't
    /// pick up unrelated audio just to bump match count.
    const SKIP_ASR_PENALTY: f32 = -0.05;
    /// Cost of skipping a reference word (singer dropped it). Higher so the
    /// algorithm prefers consuming ref content over discarding it.
    const SKIP_REF_PENALTY: f32 = -0.5;

    // Flatten reference into (line_idx, normalized_word) pairs, preserving
    // line order. Empty lines contribute no words.
    let mut ref_pairs: Vec<(usize, String)> = Vec::new();
    for (l, line) in ref_lines.iter().enumerate() {
        for w in line.split_whitespace() {
            let n = normalize_word(w);
            if !n.is_empty() {
                ref_pairs.push((l, n));
            }
        }
    }
    let n = ref_pairs.len();
    let m = asr_words.len();

    if n == 0 || m == 0 {
        return ref_lines
            .iter()
            .map(|t| LineEmit {
                text: t.clone(),
                asr_word_indices: Vec::new(),
            })
            .collect();
    }

    // DP table + back-pointers. bt: 0 = match (came from i-1, j-1),
    // 1 = skip_asr (came from i, j-1), 2 = skip_ref (came from i-1, j).
    let mut dp: Vec<Vec<f32>> = vec![vec![0.0; m + 1]; n + 1];
    let mut bt: Vec<Vec<u8>> = vec![vec![0u8; m + 1]; n + 1];

    // Initialize boundaries.
    // dp[0][j] = 0: leading audio (intro / silence / instrumental) is free.
    for j in 0..=m {
        dp[0][j] = 0.0;
        bt[0][j] = 1; // skip_asr
    }
    // dp[i][0]: dropping leading ref words is expensive; keep cumulative
    // SKIP_REF_PENALTY so the path is incentivized to find an audio anchor.
    for i in 1..=n {
        dp[i][0] = dp[i - 1][0] + SKIP_REF_PENALTY;
        bt[i][0] = 2; // skip_ref
    }

    for i in 1..=n {
        for j in 1..=m {
            let m_score = if ref_pairs[i - 1].1 == asr_words[j - 1].norm {
                dp[i - 1][j - 1] + MATCH_BONUS
            } else {
                f32::NEG_INFINITY
            };
            let s_asr = dp[i][j - 1] + SKIP_ASR_PENALTY;
            let s_ref = dp[i - 1][j] + SKIP_REF_PENALTY;

            // Pick max; ties broken in MATCH > SKIP_ASR > SKIP_REF order so
            // the alignment prefers consuming both sides when possible.
            if m_score >= s_asr && m_score >= s_ref {
                dp[i][j] = m_score;
                bt[i][j] = 0;
            } else if s_asr >= s_ref {
                dp[i][j] = s_asr;
                bt[i][j] = 1;
            } else {
                dp[i][j] = s_ref;
                bt[i][j] = 2;
            }
        }
    }

    // Traceback from (n, m). Collect MATCH pairs; ignore the rest.
    let mut indices_per_line: Vec<Vec<usize>> = vec![Vec::new(); ref_lines.len()];
    let mut i = n;
    let mut j = m;
    while i > 0 && j > 0 {
        match bt[i][j] {
            0 => {
                let line_idx = ref_pairs[i - 1].0;
                indices_per_line[line_idx].push(j - 1);
                i -= 1;
                j -= 1;
            }
            1 => j -= 1,
            2 => i -= 1,
            _ => break,
        }
    }
    while j > 0 {
        j -= 1;
    }
    while i > 0 {
        i -= 1;
    }

    // Sort each line's indices ascending (traceback adds them in reverse).
    for v in indices_per_line.iter_mut() {
        v.sort_unstable();
    }

    ref_lines
        .iter()
        .zip(indices_per_line)
        .map(|(text, indices)| LineEmit {
            text: text.clone(),
            asr_word_indices: indices,
        })
        .collect()
}

// ── Phase 2: chorus repeat detection ──────────────────────────────────────────

fn detect_chorus_repeats(
    ref_lines: &[String],
    asr_words: &[AsrWord],
    emits: &[LineEmit],
) -> Vec<LineEmit> {
    // Build set of ASR indices already consumed by Phase 1.
    let mut consumed: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for e in emits {
        for &i in &e.asr_word_indices {
            consumed.insert(i);
        }
    }

    // Find runs of consecutive unconsumed ASR indices.
    let mut gaps: Vec<(usize, usize)> = Vec::new(); // (start_idx, end_idx_inclusive)
    let mut cur_start: Option<usize> = None;
    for i in 0..asr_words.len() {
        if !consumed.contains(&i) {
            cur_start.get_or_insert(i);
        } else if let Some(s) = cur_start.take() {
            gaps.push((s, i - 1));
        }
    }
    if let Some(s) = cur_start {
        gaps.push((s, asr_words.len() - 1));
    }

    // Filter gaps by duration.
    let long_gaps: Vec<(usize, usize)> = gaps
        .into_iter()
        .filter(|&(s, e)| {
            asr_words[e].end_ms.saturating_sub(asr_words[s].start_ms) >= CHORUS_REPEAT_GAP_MS
        })
        .collect();

    // Pre-tokenize each ref line once. Empty ref lines (rare; defensive)
    // contribute zero words so they're skipped in the per-gap loop.
    let ref_norms_per_line: Vec<Vec<String>> = ref_lines
        .iter()
        .map(|line| {
            line.split_whitespace()
                .map(normalize_word)
                .filter(|s| !s.is_empty())
                .collect()
        })
        .collect();

    let mut extras = Vec::new();
    for (gap_s, gap_e) in long_gaps {
        // Worship songs typically repeat the chorus 3-4 times in a row during
        // bridges / vamps. A gap can therefore correspond to MULTIPLE chorus
        // re-emissions, not just one. We loop the matcher: each iteration
        // picks the best-scoring ref line in the gap's currently-unconsumed
        // audio words; if its score >= threshold, we emit and remove its
        // matched indices from the unconsumed set, then re-scan. Stop when no
        // ref line qualifies, or unconsumed becomes too short to score.
        //
        // Without this loop a 90-second instrumental that sings the chorus 3×
        // would only get 1 re-emit (one chorus printed; the wall blank for
        // the other 60+ seconds). The 2026-05-03 wall verification on
        // id=132 (Holy Forever) surfaced the gap.
        let mut unconsumed: std::collections::BTreeSet<usize> = (gap_s..=gap_e).collect();

        loop {
            if unconsumed.is_empty() {
                break;
            }
            let gap_indices: Vec<usize> = unconsumed.iter().copied().collect();
            let gap_norms: Vec<&str> = gap_indices
                .iter()
                .map(|&i| asr_words[i].norm.as_str())
                .collect();

            let mut best: Option<(usize, f32, Vec<usize>)> = None;
            for (li, ref_norms) in ref_norms_per_line.iter().enumerate() {
                if ref_norms.is_empty() {
                    continue;
                }
                let ref_strs: Vec<&str> = ref_norms.iter().map(|s| s.as_str()).collect();
                let alignment = lcs_align(&ref_strs, &gap_norms);
                let matched: Vec<usize> = alignment
                    .iter()
                    .filter_map(|a| a.map(|j| gap_indices[j]))
                    .collect();
                let score = matched.len() as f32 / ref_norms.len() as f32;
                if matched.len() < CHORUS_REPEAT_MIN_MATCHED_WORDS {
                    continue;
                }
                // Reject re-emits whose matched audio span is too short to
                // display (single-word matches, near-instant collapse). The
                // floor-clamp in Phase 5 would still produce a 1-ms window
                // that flashes invisibly on the wall.
                let span_ms = match (matched.iter().min(), matched.iter().max()) {
                    (Some(&imin), Some(&imax)) => asr_words[imax]
                        .end_ms
                        .saturating_sub(asr_words[imin].start_ms),
                    _ => 0,
                };
                if span_ms < MIN_LINE_DURATION_MS {
                    continue;
                }
                if score >= CHORUS_REPEAT_MIN_MATCH_RATIO
                    && best.as_ref().is_none_or(|(_, s, _)| score > *s)
                {
                    best = Some((li, score, matched));
                }
            }

            match best {
                Some((li, score, matched)) => {
                    debug!(
                        ref_idx = li,
                        score,
                        gap_start_ms = asr_words[*matched.iter().min().unwrap_or(&gap_s)].start_ms,
                        gap_end_ms = asr_words[*matched.iter().max().unwrap_or(&gap_e)].end_ms,
                        "description_merge: re-emit chorus repeat"
                    );
                    for &idx in &matched {
                        unconsumed.remove(&idx);
                    }
                    extras.push(LineEmit {
                        text: ref_lines[li].clone(),
                        asr_word_indices: matched,
                    });
                }
                None => break,
            }
        }
    }

    extras
}

// ── Phase 3: Claude-driven natural-phrase splits ──────────────────────────────

#[derive(Debug, Deserialize)]
struct ClaudeSplitsResponse {
    splits: Vec<ClaudeSplitEntry>,
}

#[derive(Debug, Deserialize)]
struct ClaudeSplitEntry {
    i: usize,
    subs: Vec<ClaudeSubLine>,
}

#[derive(Debug, Deserialize)]
struct ClaudeSubLine {
    en: String,
}

async fn claude_split_lines(
    ai_client: &AiClient,
    long_lines: &[(usize, &str)],
) -> Result<HashMap<usize, Vec<String>>, anyhow::Error> {
    if long_lines.is_empty() {
        return Ok(HashMap::new());
    }
    let prompt = build_split_prompt(long_lines);
    let raw = ai_client.chat("", &prompt).await?;
    let parsed = parse_split_response(&raw)?;

    let mut map: HashMap<usize, Vec<String>> = HashMap::new();
    for entry in parsed.splits {
        let subs: Vec<String> = entry.subs.iter().map(|s| s.en.clone()).collect();
        let all_fit = subs.iter().all(|s| s.chars().count() <= SUBLINE_MAX_CHARS);
        if !all_fit {
            // Claude violated the hard cap on at least one sub. Fall back to
            // deterministic split for this line — partial trust.
            warn!(
                index = entry.i,
                "description_merge: claude returned sub-line over {} chars, falling back deterministic for this line",
                SUBLINE_MAX_CHARS
            );
            continue;
        }
        if !subs.is_empty() {
            map.insert(entry.i, subs);
        }
    }

    // For any long line Claude failed to return: deterministic fallback.
    for (i, text) in long_lines {
        if !map.contains_key(i) {
            map.insert(*i, deterministic_split_one(text));
        }
    }
    Ok(map)
}

fn build_split_prompt(long_lines: &[(usize, &str)]) -> String {
    let input_repr = long_lines
        .iter()
        .map(|(i, text)| {
            format!(
                "{}. ({}c) {}",
                i,
                text.chars().count(),
                serde_json::to_string(text).unwrap_or_else(|_| format!("{text:?}"))
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"You receive worship-song lines for LED-wall karaoke display.

HARD CONSTRAINT: every output sub-line MUST be <= 32 characters. The LED wall renders only 32 chars per row; longer lines visually overflow into adjacent UI panels and are unacceptable.

Task: for each input line, return the FEWEST sub-lines that all fit the 32-char cap. Pick split points at natural sung phrase boundaries — where the singer breathes. NOT mechanical char counting.

Hierarchy of preferred split points (apply highest-priority that respects 32-char cap):
1. After punctuation that marks a clause break: "." "!" "?" ";" ":"
2. Before a connective word: "and", "but", "or", "yet", "so" (split BEFORE the word — "...gone before us / and all who will believe").
3. After comma "," when both halves read as separate phrases.
4. Before a prepositional phrase: "of", "in", "to", "at", "from", "with" (split BEFORE the preposition).
5. Word boundary nearest the middle of the 32-char window — last resort.

Sub-line count rules:
- 2 sub-lines for EN 33-64 chars (default for nearly every worship-line case).
- 3 sub-lines ONLY if EN > 64 chars AND there are two clear phrase boundaries.

Preserve EXACT punctuation and capitalization from input.

Input lines:
{input_repr}

Output: ONLY a JSON object. Schema:
{{"splits": [{{"i": <input index>, "subs": [{{"en": "<sub-line>"}}]}}]}}

EVERY input line must appear with at least one sub. EVERY output `en` MUST be <= 32 chars.
First char of response = `{{`. No prose, no fences."#
    )
}

fn parse_split_response(raw: &str) -> Result<ClaudeSplitsResponse, anyhow::Error> {
    // Find first balanced JSON object.
    let s = raw.trim();
    let bytes = s.as_bytes();
    let mut depth: i32 = 0;
    let mut start: Option<usize> = None;
    let mut end: Option<usize> = None;
    let mut in_str = false;
    let mut esc = false;
    for (i, &b) in bytes.iter().enumerate() {
        if esc {
            esc = false;
            continue;
        }
        if in_str && b == b'\\' {
            esc = true;
            continue;
        }
        if b == b'"' {
            in_str = !in_str;
            continue;
        }
        if in_str {
            continue;
        }
        if b == b'{' {
            if start.is_none() {
                start = Some(i);
            }
            depth += 1;
        } else if b == b'}' {
            depth -= 1;
            if depth == 0 {
                end = Some(i + 1);
                break;
            }
        }
    }
    let (s_idx, e_idx) = match (start, end) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            return Err(anyhow::anyhow!(
                "no balanced JSON object in claude response"
            ));
        }
    };
    let json_slice = &s[s_idx..e_idx];
    Ok(serde_json::from_str(json_slice)?)
}

fn deterministic_split_lines(long_lines: &[(usize, &str)]) -> HashMap<usize, Vec<String>> {
    long_lines
        .iter()
        .map(|(i, text)| (*i, deterministic_split_one(text)))
        .collect()
}

fn deterministic_split_one(text: &str) -> Vec<String> {
    // Recursively split until each piece <= SUBLINE_MAX_CHARS.
    let mut out = Vec::new();
    deterministic_split_recurse(text.trim(), &mut out);
    if out.is_empty() {
        out.push(text.to_string());
    }
    out
}

fn deterministic_split_recurse(text: &str, out: &mut Vec<String>) {
    if text.chars().count() <= SUBLINE_MAX_CHARS {
        out.push(text.to_string());
        return;
    }
    // Look for a split point in priority order.
    let chars: Vec<char> = text.chars().collect();
    let cap = chars.len().min(SUBLINE_MAX_CHARS);

    // 1. Sentence-end punctuation rightmost <= cap.
    if let Some(idx) = rfind_in(&chars[..cap], &['.', '!', '?']) {
        let split_byte = char_to_byte(text, idx + 1);
        let (l, r) = text.split_at(split_byte);
        deterministic_split_recurse(l.trim(), out);
        deterministic_split_recurse(r.trim(), out);
        return;
    }
    // 2. Comma / semicolon / colon rightmost <= cap.
    if let Some(idx) = rfind_in(&chars[..cap], &[',', ';', ':']) {
        let split_byte = char_to_byte(text, idx + 1);
        let (l, r) = text.split_at(split_byte);
        deterministic_split_recurse(l.trim(), out);
        deterministic_split_recurse(r.trim(), out);
        return;
    }
    // 3. Word boundary closest to middle within cap.
    let mid = cap / 2;
    let mut best: Option<usize> = None;
    let mut best_dist: Option<usize> = None;
    for i in 1..cap {
        if chars[i] == ' ' {
            let d = mid.abs_diff(i);
            if best_dist.is_none_or(|bd| d < bd) {
                best_dist = Some(d);
                best = Some(i);
            }
        }
    }
    if let Some(idx) = best {
        let split_byte = char_to_byte(text, idx);
        let (l, r) = text.split_at(split_byte);
        deterministic_split_recurse(l.trim(), out);
        deterministic_split_recurse(r.trim(), out);
        return;
    }
    // 4. No split possible — emit as-is even if over cap (degenerate).
    out.push(text.to_string());
}

fn rfind_in(chars: &[char], targets: &[char]) -> Option<usize> {
    chars.iter().rposition(|c| targets.contains(c))
}

fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

// ── Phase 4: emit AlignedLine with sub-line word timing ───────────────────────

fn aligned_lines_for_emit(
    emit: &LineEmit,
    asr_words: &[AsrWord],
    subs: Option<&Vec<String>>,
) -> Vec<AlignedLine> {
    match subs {
        None => vec![emit_single(emit, asr_words)],
        Some(sub_texts) if sub_texts.len() == 1 => {
            let mut e = emit.clone();
            e.text = sub_texts[0].clone();
            vec![emit_single(&e, asr_words)]
        }
        Some(sub_texts) => emit_with_subs(emit, asr_words, sub_texts),
    }
}

fn emit_single(emit: &LineEmit, asr_words: &[AsrWord]) -> AlignedLine {
    let (s, e) = match (
        emit.asr_word_indices.iter().min(),
        emit.asr_word_indices.iter().max(),
    ) {
        (Some(&imin), Some(&imax)) => (asr_words[imin].start_ms, asr_words[imax].end_ms),
        _ => (0, 0), // unmatched line; floor-clamped later
    };
    AlignedLine {
        text: emit.text.clone(),
        start_ms: s,
        end_ms: e,
        words: None,
    }
}

fn emit_with_subs(
    emit: &LineEmit,
    asr_words: &[AsrWord],
    sub_texts: &[String],
) -> Vec<AlignedLine> {
    // Second LCS within parent's matched ASR word range. Build the parent's
    // ASR sub-stream and LCS-align each sub-line's normalized words to it.
    let mut parent_indices = emit.asr_word_indices.clone();
    parent_indices.sort_unstable();
    if parent_indices.is_empty() {
        // No matched audio words — fall back to evenly-distributed ZERO-window
        // sub-lines that the floor-clamp will spread out.
        return sub_texts
            .iter()
            .map(|t| AlignedLine {
                text: t.clone(),
                start_ms: 0,
                end_ms: 0,
                words: None,
            })
            .collect();
    }

    // Build a sub-line word-index range via LCS.
    let parent_norms: Vec<&str> = parent_indices
        .iter()
        .map(|&i| asr_words[i].norm.as_str())
        .collect();

    let mut sub_aligned: Vec<AlignedLine> = Vec::with_capacity(sub_texts.len());
    let mut search_start = 0usize; // position within parent_indices
    for (si, sub_text) in sub_texts.iter().enumerate() {
        let sub_norms: Vec<String> = sub_text
            .split_whitespace()
            .map(normalize_word)
            .filter(|s| !s.is_empty())
            .collect();
        let sub_strs: Vec<&str> = sub_norms.iter().map(|s| s.as_str()).collect();
        let parent_window: Vec<&str> = parent_norms[search_start..].to_vec();
        let alignment = lcs_align(&sub_strs, &parent_window);
        let matched_in_window: Vec<usize> = alignment
            .iter()
            .filter_map(|a| a.map(|j| search_start + j))
            .collect();

        let (s_ms, e_ms) = if let (Some(&imin), Some(&imax)) = (
            matched_in_window.iter().min(),
            matched_in_window.iter().max(),
        ) {
            let s = asr_words[parent_indices[imin]].start_ms;
            let e = asr_words[parent_indices[imax]].end_ms;
            (s, e)
        } else {
            // Sub couldn't be matched; use a placeholder around the parent's
            // proportional time. Will be floor-clamped by Phase 5.
            let total_subs = sub_texts.len() as u32;
            let parent_start = asr_words[parent_indices[0]].start_ms;
            let parent_end = asr_words[parent_indices[parent_indices.len() - 1]].end_ms;
            let parent_dur = parent_end.saturating_sub(parent_start);
            let unit = parent_dur / total_subs.max(1);
            let s = parent_start + unit * si as u32;
            let e = s + unit;
            (s, e)
        };

        sub_aligned.push(AlignedLine {
            text: sub_text.clone(),
            start_ms: s_ms,
            end_ms: e_ms,
            words: None,
        });

        // Advance search_start past the LAST matched parent index so the next
        // sub starts looking after this one.
        if let Some(&imax) = matched_in_window.iter().max() {
            search_start = imax + 1;
        }
    }

    sub_aligned
}

// ── Phase 5: cap + monotonic ──────────────────────────────────────────────────

fn apply_cap_and_monotonic(lines: &mut Vec<AlignedLine>) {
    // Sort by start_ms ascending; ties broken by original order (stable sort).
    lines.sort_by_key(|l| l.start_ms);

    // Drop micro-windows whose original duration is below MIN_LINE_DURATION_MS.
    // The previous policy inflated them up to MIN_LINE_DURATION_MS, but a
    // 500 ms emit reads as a flash on the wall (operator wall-verify on
    // id=132 2026-05-04). Better to leave the wall blank than flash a
    // sub-readable line. Affects Phase 2 chorus repeats with tiny matched
    // spans and Phase 4 sub-line splits where the second LCS finds
    // near-zero overlap.
    let mut output = Vec::with_capacity(lines.len());
    let mut floor: u32 = 0;
    for mut l in std::mem::take(lines) {
        // Pre-clamp duration check.
        let dur = l.end_ms.saturating_sub(l.start_ms);
        if dur < MIN_LINE_DURATION_MS {
            continue;
        }
        // Upper cap.
        if dur > LONG_LINE_CAP_MS {
            l.end_ms = l.start_ms.saturating_add(LONG_LINE_CAP_MS);
        }
        // Floor-clamp start_ms forward to enforce monotonic non-overlap.
        if l.start_ms < floor {
            l.start_ms = floor;
        }
        // Drop if floor-clamp collapsed the window below threshold.
        if l.end_ms.saturating_sub(l.start_ms) < MIN_LINE_DURATION_MS {
            continue;
        }
        floor = l.end_ms;
        output.push(l);
    }
    *lines = output;
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Trim trailing-outlier indices so derived span ≤ `LONG_LINE_CAP_MS`.
/// LCS matchers can pick a far-later word that fits the ref-line pattern
/// but lies past the real sung instance — id=132 2026-05-04: 5 contiguous
/// words + 6th from a different chorus, spanning 11.8 s. Trim keeps tail
/// off until span fits cap; never drops below 2 (CHORUS_REPEAT_MIN floor).
fn trim_outlier_indices(indices: &mut Vec<usize>, asr_words: &[AsrWord]) {
    if indices.len() <= 2 {
        return;
    }
    indices.sort_unstable();
    while indices.len() > 2 {
        let first = indices[0];
        let last = *indices.last().expect("len > 2");
        let span = asr_words[last]
            .end_ms
            .saturating_sub(asr_words[first].start_ms);
        if span <= LONG_LINE_CAP_MS {
            break;
        }
        indices.pop();
    }
}

fn normalize_word(w: &str) -> String {
    w.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn lcs_align(ref_words: &[&str], asr_words: &[&str]) -> Vec<Option<usize>> {
    let n = ref_words.len();
    let m = asr_words.len();
    if n == 0 || m == 0 {
        return vec![None; n];
    }
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in 0..n {
        for j in 0..m {
            dp[i + 1][j + 1] = if ref_words[i] == asr_words[j] {
                dp[i][j] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut alignment = vec![None; n];
    let mut i = n;
    let mut j = m;
    while i > 0 && j > 0 {
        if ref_words[i - 1] == asr_words[j - 1] {
            alignment[i - 1] = Some(j - 1);
            i -= 1;
            j -= 1;
        } else if dp[i - 1][j] >= dp[i][j - 1] {
            i -= 1;
        } else {
            j -= 1;
        }
    }
    alignment
}

fn emit_unmatched_only(asr: &AlignedTrack, candidate: &CandidateText) -> AlignedTrack {
    // Audio had no usable word timings — ship reference text with placeholder
    // 1s windows starting at 0. Wall will display lines but timing is bogus;
    // operator can review and add manual timings later.
    let mut out = Vec::with_capacity(candidate.lines.len());
    let mut t = 0u32;
    for line in &candidate.lines {
        out.push(AlignedLine {
            text: line.clone(),
            start_ms: t,
            end_ms: t + 1000,
            words: None,
        });
        t += 1000;
    }
    AlignedTrack {
        lines: out,
        provenance: format!("{}+{}", candidate.source, asr.provenance),
        raw_confidence: 0.0,
    }
}

#[cfg(test)]
#[path = "description_merge_tests.rs"]
mod tests;
