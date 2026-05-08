//! Text-reference merge pipeline (issue #78 + 2026-05-07 unification). Phases:
//! 1 Claude line-mapping (NW DP fallback), 2 chorus repeat via sliding-
//! window LCS, 2.5 trim outliers, 2.7 absorb sustained-note tokens,
//! 3 Claude split >32c, 4 emit AlignedLine, 5 cap + monotonic + extend.
//! `words: None` (feedback_line_timing_only). Provenance prefix from
//! source candidate, no `+claude-merge` suffix.

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

#[path = "text_reference_merge_absorb.rs"]
mod absorb;
#[path = "text_reference_merge_added.rs"]
mod added;
#[path = "text_reference_merge_audit.rs"]
mod audit;
#[path = "text_reference_merge_mapping.rs"]
mod mapping;
#[path = "text_reference_merge_phantom.rs"]
mod phantom;
#[path = "text_reference_merge_trim.rs"]
mod trim;
#[path = "text_reference_merge_window.rs"]
mod window;

/// LED wall char/row cap; longer lines overflow.
pub const SUBLINE_MAX_CHARS: usize = 32;

/// Cap on a line's display duration; longer = wall goes blank.
pub const LONG_LINE_CAP_MS: u32 = 8000;

/// Gap between matched lines that triggers chorus-repeat detection.
const CHORUS_REPEAT_GAP_MS: u32 = 4000;

/// Min word match ratio (matched/ref) for chorus re-emit.
const CHORUS_REPEAT_MIN_MATCH_RATIO: f32 = 0.6;

/// Min ASR words matched. Floors ratio so a 2-word ref needs 2 hits.
const CHORUS_REPEAT_MIN_MATCHED_WORDS: usize = 2;

/// Min display duration; below this collapses to invisible flashes.
const MIN_LINE_DURATION_MS: u32 = 500;

/// Phase 5 extension. Small-gap full pull-back; large-gap pull by
/// EXTENSION_TOLERANCE_MS (instrumental silence in middle).
const EXTENSION_TOLERANCE_MS: u32 = 1500;
const REASONABLE_GAP_MS: u32 = 4000;

/// One ref-line emission + matched ASR word indices (Phase 1 or 2 re-emit).
#[derive(Clone, Debug)]
struct LineEmit {
    text: String,
    /// Indices into the global flattened `asr_words` vec.
    asr_word_indices: Vec<usize>,
}

#[derive(Clone, Debug)]
pub(crate) struct AsrWord {
    pub(crate) norm: String,
    pub(crate) start_ms: u32,
    pub(crate) end_ms: u32,
    pub(crate) confidence: f32,
}

/// Public entry: full description/override pipeline. Output: words=None,
/// EN ≤32c, line ≤8s, chorus repeats re-emitted.
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
    // also returns added_ref_lines for missing-section gaps (≥ 5 s
    // unmatched audio runs the description omits — id=21 "Good Shepherd"
    // bridge + outro). These are inserted into an expanded reference list
    // and aligned in Phase 1.5 below.
    let (mut emits, phase1_provider, expanded_ref_lines, added_ref_lines): (
        Vec<LineEmit>,
        &str,
        Vec<String>,
        Vec<mapping::AddedRefLine>,
    ) = match mapping::claude_map_words_to_lines(ai_client, ref_lines, &asr_words).await {
        Ok(result) => {
            info!(
                ref_lines = ref_lines.len(),
                asr_words = asr_words.len(),
                added_ref_lines = result.added.len(),
                "text_reference_merge: claude line-mapping succeeded"
            );
            let (expanded, orig_to_expanded) = expand_ref_lines(ref_lines, &result.added);
            let remapped = remap_mapping(&result.mapping, &orig_to_expanded);
            (
                mapping::emits_from_mapping(&remapped, &expanded),
                "claude",
                expanded,
                result.added,
            )
        }
        Err(e) => {
            warn!(
                %e,
                ref_lines = ref_lines.len(),
                asr_words = asr_words.len(),
                "text_reference_merge: claude line-mapping failed; falling back to NW DP"
            );
            (
                match_ref_to_asr(ref_lines, &asr_words),
                "nw_dp",
                ref_lines.to_vec(),
                Vec::new(),
            )
        }
    };
    audit_state.record_phase1(phase1_provider, &emits, &asr_words);
    audit_state.record_phase1_added_ref_lines(&added_ref_lines);

    // Phase 1.5: align added reference lines against unmatched ASR-word
    // windows (one per added line). Added emits join Phase 1 emits and
    // flow through Phases 2/2.5/2.6/2.7/3/4/5 unchanged.
    if !added_ref_lines.is_empty() {
        let (_, orig_to_expanded) = expand_ref_lines(ref_lines, &added_ref_lines);
        let added_expanded_indices: Vec<usize> = {
            let mut counts: std::collections::HashMap<usize, usize> = Default::default();
            added_ref_lines
                .iter()
                .map(|a| {
                    let base = orig_to_expanded[a.after_line];
                    let k = *counts.get(&a.after_line).unwrap_or(&0);
                    counts.insert(a.after_line, k + 1);
                    base + 1 + k
                })
                .collect()
        };
        let added_emits = added::align_added_lines(
            &expanded_ref_lines,
            &added_ref_lines,
            &added_expanded_indices,
            &orig_to_expanded,
            &asr_words,
            &emits,
        );
        info!(
            count = added_emits.len(),
            "text_reference_merge: phase 1.5 added-line emits"
        );
        emits.extend(added_emits);
    }

    // From here on, `ref_lines` refers to the expanded reference list.
    let ref_lines: &[String] = &expanded_ref_lines;

    // Phase 2: chorus repeat re-emit for long unmatched gaps.
    let extras = detect_chorus_repeats(ref_lines, &asr_words, &emits);
    if !extras.is_empty() {
        info!(count = extras.len(), "text_reference_merge: chorus repeats");
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

    // Phase 2.6: prefix-absorption — attach unconsumed prefix words that
    // Phase 2's window cap missed (id=132 3:07).
    absorb::absorb_prefix_matches(&mut emits, &asr_words);

    // Phase 2.7: sustained-note absorption — same-text boundary tokens
    // stay with prev line so wall doesn't switch mid-sustained-note.
    absorb::absorb_sustained_boundary_tokens(&mut emits, &asr_words);

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
                    "text_reference_merge: claude split failed, falling back to deterministic word-boundary split"
                );
                deterministic_split_lines(&needs_split)
            }
        }
    };

    // Phase 4: emit AlignedLine. Skip emits with no matched ASR words.
    let mut output: Vec<AlignedLine> = Vec::new();
    for (i, emit) in emits.iter().enumerate() {
        if emit.asr_word_indices.is_empty() {
            continue;
        }
        let lines = aligned_lines_for_emit(emit, &asr_words, split_map.get(&i));
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
                    confidence: w.confidence,
                });
            }
        }
    }
    let _ = phantom::drop_phantom_clusters(&mut out);
    out
}

/// NW DP fallback for Phase 1 when Claude fails. Globally optimal alignment
/// of flattened reference words against ASR word stream; traceback groups
/// matched (ref_word_idx, asr_word_idx) pairs back by ref line. See git log
/// for the full algorithm description.
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

    // bt: 0 = match, 1 = skip_asr, 2 = skip_ref.
    let mut dp: Vec<Vec<f32>> = vec![vec![0.0; m + 1]; n + 1];
    let mut bt: Vec<Vec<u8>> = vec![vec![0u8; m + 1]; n + 1];

    for j in 0..=m {
        dp[0][j] = 0.0;
        bt[0][j] = 1;
    }
    for i in 1..=n {
        dp[i][0] = dp[i - 1][0] + SKIP_REF_PENALTY;
        bt[i][0] = 2;
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
        let mut unconsumed: Vec<usize> = (gap_s..=gap_e).collect();

        loop {
            if unconsumed.is_empty() {
                break;
            }
            let best =
                window::best_window_match(&ref_norms_per_line, &unconsumed, asr_words, &lcs_align);
            match best {
                Some((li, score, matched)) => {
                    debug!(
                        ref_idx = li,
                        score,
                        win_start_ms = asr_words[*matched.first().expect("non-empty")].start_ms,
                        win_end_ms = asr_words[*matched.last().expect("non-empty")].end_ms,
                        "text_reference_merge: re-emit chorus repeat (window-bounded)"
                    );
                    let consumed: std::collections::HashSet<usize> =
                        matched.iter().copied().collect();
                    unconsumed.retain(|i| !consumed.contains(i));
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
                "text_reference_merge: claude returned sub-line over {} chars, falling back deterministic for this line",
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
    let chars: Vec<char> = text.chars().collect();
    let cap = chars.len().min(SUBLINE_MAX_CHARS);
    if let Some(idx) = rfind_in(&chars[..cap], &['.', '!', '?']) {
        let split_byte = char_to_byte(text, idx + 1);
        let (l, r) = text.split_at(split_byte);
        deterministic_split_recurse(l.trim(), out);
        deterministic_split_recurse(r.trim(), out);
        return;
    }
    if let Some(idx) = rfind_in(&chars[..cap], &[',', ';', ':']) {
        let split_byte = char_to_byte(text, idx + 1);
        let (l, r) = text.split_at(split_byte);
        deterministic_split_recurse(l.trim(), out);
        deterministic_split_recurse(r.trim(), out);
        return;
    }
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

        if let Some(&imax) = matched_in_window.iter().max() {
            search_start = imax + 1;
        }
    }

    sub_aligned
}

// ── Phase 5: cap + monotonic ──────────────────────────────────────────────────

fn apply_cap_and_monotonic(lines: &mut Vec<AlignedLine>) {
    lines.sort_by_key(|l| l.start_ms);
    let natural_ends: Vec<u32> = lines.iter().map(|l| l.end_ms).collect();
    let mut floor: u32 = 0;
    for l in lines.iter_mut() {
        if l.start_ms < floor {
            l.start_ms = floor;
        }
        floor = l.end_ms;
    }

    let n = lines.len();
    for i in 0..n.saturating_sub(1) {
        let next_start = lines[i + 1].start_ms;
        let natural_gap = next_start.saturating_sub(natural_ends[i]);
        if natural_gap <= REASONABLE_GAP_MS {
            let new_next_start = natural_ends[i];
            if new_next_start < lines[i + 1].start_ms {
                lines[i + 1].start_ms = new_next_start;
            }
            if natural_ends[i] > lines[i].end_ms {
                lines[i].end_ms = natural_ends[i];
            }
        } else {
            let new_next_start = next_start
                .saturating_sub(EXTENSION_TOLERANCE_MS)
                .max(natural_ends[i]);
            if new_next_start < lines[i + 1].start_ms {
                lines[i + 1].start_ms = new_next_start;
            }
            let new_end = natural_ends[i]
                .saturating_add(EXTENSION_TOLERANCE_MS)
                .min(lines[i + 1].start_ms);
            if new_end > lines[i].end_ms {
                lines[i].end_ms = new_end;
            }
        }
    }

    lines.retain(|l| l.end_ms.saturating_sub(l.start_ms) >= MIN_LINE_DURATION_MS);
}

pub(crate) use trim::trim_outlier_indices;

fn normalize_word(w: &str) -> String {
    w.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Forward-greedy + DP, pick whichever has more matches; tie → forward-
/// greedy. Forward-greedy fixes 2:35 (line 1 takes first dup, line 2
/// takes second). DP fixes 3:07 (end-anchored long-ref match).
fn lcs_align(ref_words: &[&str], asr_words: &[&str]) -> Vec<Option<usize>> {
    let fg = lcs_align_forward_greedy(ref_words, asr_words);
    let dp = lcs_align_dp(ref_words, asr_words);
    let fg_count = fg.iter().filter(|x| x.is_some()).count();
    let dp_count = dp.iter().filter(|x| x.is_some()).count();
    if fg_count >= dp_count { fg } else { dp }
}

fn lcs_align_forward_greedy(ref_words: &[&str], asr_words: &[&str]) -> Vec<Option<usize>> {
    let n = ref_words.len();
    let m = asr_words.len();
    let mut alignment = vec![None; n];
    if n == 0 || m == 0 {
        return alignment;
    }
    let mut j = 0;
    for i in 0..n {
        while j < m && ref_words[i] != asr_words[j] {
            j += 1;
        }
        if j < m {
            alignment[i] = Some(j);
            j += 1;
        } else {
            break;
        }
    }
    alignment
}

fn lcs_align_dp(ref_words: &[&str], asr_words: &[&str]) -> Vec<Option<usize>> {
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

/// Expanded ref-list + `original → expanded` index map.
pub(crate) fn expand_ref_lines(
    original: &[String],
    added: &[mapping::AddedRefLine],
) -> (Vec<String>, Vec<usize>) {
    let mut expanded: Vec<String> = Vec::with_capacity(original.len() + added.len());
    let mut orig_to_expanded: Vec<usize> = Vec::with_capacity(original.len());
    for (i, line) in original.iter().enumerate() {
        orig_to_expanded.push(expanded.len());
        expanded.push(line.clone());
        for a in added.iter().filter(|a| a.after_line == i) {
            expanded.push(a.text.clone());
        }
    }
    (expanded, orig_to_expanded)
}

/// Rewrite Phase 1 mapping into expanded ref-list indices.
pub(crate) fn remap_mapping(
    original_map: &[Option<usize>],
    orig_to_expanded: &[usize],
) -> Vec<Option<usize>> {
    original_map
        .iter()
        .map(|opt| opt.and_then(|li| orig_to_expanded.get(li).copied()))
        .collect()
}

#[cfg(test)]
#[path = "text_reference_merge_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "text_reference_merge_trim_tests.rs"]
mod trim_tests;

#[cfg(test)]
#[path = "text_reference_merge_phantom_tests.rs"]
mod phantom_tests;

#[cfg(test)]
#[path = "text_reference_merge_dp_tests.rs"]
mod dp_tests;

#[cfg(test)]
#[path = "text_reference_merge_split_tests.rs"]
mod split_tests;
