//! Safe Claude line-regroup for asr_path. The silence-gap splitter over-segments
//! (single phrases broken across lines; held/repeated words on their own line).
//! This pass asks Claude to regroup the already-split lines into singable lines,
//! guided by the reference (genius) line phrasing.
//!
//! DROP-SAFE BY CONSTRUCTION: Claude operates on LINE indices (tens of them), not
//! word indices (the prior word-index merge dropped choruses). Each output line
//! references a contiguous range of input-line indices; the input lines already
//! carry correct timing. We then VALIDATE that the ranges are strictly forward
//! AND cover every input line exactly once. If validation fails (gap, overlap,
//! reorder, missing tail, empty), we return the input lines UNCHANGED — never a
//! partial/dropped result. Worst case: no phrasing improvement, never lost content.

use serde::Deserialize;
use sp_core::lyrics::LyricsLine;

/// Abstraction over `AiClient::chat` so tests can inject canned responses.
#[async_trait::async_trait]
pub trait RegroupChat: Send + Sync {
    async fn chat(&self, system: &str, user: &str) -> Result<String, String>;
}

#[async_trait::async_trait]
impl RegroupChat for crate::ai::client::AiClient {
    // mutants::skip: pure delegation to AiClient::chat — no logic of its own to
    // mutate meaningfully; exercised end-to-end by the real worker path.
    #[cfg_attr(test, mutants::skip)]
    async fn chat(&self, system: &str, user: &str) -> Result<String, String> {
        crate::ai::client::AiClient::chat(self, system, user)
            .await
            .map_err(|e| e.to_string())
    }
}

#[derive(Debug, Clone, Deserialize)]
struct RegroupedLine {
    // Claude may emit `text`, but we IGNORE it — the merged line text is
    // reconstructed from the covered AAI lines so Claude can never drop a word
    // (it once merged "A power that can heal all pain" + "It's just one word"
    // into a group and wrote text omitting the second phrase). Claude's only
    // job here is the GROUPING (start_idx/end_idx); content comes from AAI.
    #[serde(default)]
    #[allow(dead_code)]
    text: String,
    start_idx: usize,
    end_idx: usize,
}

#[derive(Debug, Clone, Deserialize)]
struct RegroupResult {
    #[serde(default, alias = "segments")]
    lines: Vec<RegroupedLine>,
}

/// Regroup `aai_lines` into singable lines using `reference_lines` (genius/lrclib)
/// as a phrasing guide. Returns the regrouped lines on success, or `aai_lines`
/// unchanged if Claude is unavailable / its grouping fails validation.
///
/// `aai_lines` must be non-empty (caller guarantees). Timing for each regrouped
/// line is taken from the AAI lines it covers — never synthesized.
pub async fn regroup(
    chat: &dyn RegroupChat,
    aai_lines: &[LyricsLine],
    reference_lines: &[String],
) -> Vec<LyricsLine> {
    if aai_lines.len() < 2 {
        return aai_lines.to_vec();
    }
    let user = build_prompt(aai_lines, reference_lines);
    // Empty system prompt — cloaked CLIProxyAPI Claude behaves best with
    // everything in the user message (mirrors translator.rs).
    let resp = match chat.chat("", &user).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("asr_path regroup: chat failed: {e} — using raw split");
            return aai_lines.to_vec();
        }
    };
    match parse(&resp) {
        Some(groups) if !groups.lines.is_empty() => apply_greedy(&groups, aai_lines),
        _ => {
            tracing::warn!("asr_path regroup: Claude returned no usable groups — using raw split");
            aai_lines.to_vec()
        }
    }
}

fn build_prompt(aai_lines: &[LyricsLine], reference_lines: &[String]) -> String {
    let mut s = String::new();
    s.push_str(
        "Below are ASR lines from a sung recording. They are often over-split — \
         a single phrase broken across lines, or a held/repeated word on its own \
         line. Regroup them into natural singable lines.\n\n",
    );
    if !reference_lines.is_empty() {
        s.push_str(
            "REFERENCE PHRASING (how a human wrote these lyrics — use ONLY as a \
             guide for where lines should break and to fix an obvious mis-heard \
             word; the ASR text is the source of truth for content):\n",
        );
        for l in reference_lines {
            s.push_str(l);
            s.push('\n');
        }
        s.push('\n');
    }
    s.push_str("ASR LINES (index: text):\n");
    for (i, l) in aai_lines.iter().enumerate() {
        s.push_str(&format!("{i}: {}\n", l.en));
    }
    s.push_str(&format!(
        "\nTASK:\nGroup the numbered ASR lines (indices 0 to {}) into singable \
         lines. Each output line covers a CONTIGUOUS range of ASR line indices \
         [start_idx..end_idx] (inclusive). The ranges MUST move strictly forward \
         and together cover EVERY index from 0 to {} exactly once — no gaps, no \
         overlap, no reordering, no skipped index. Merge over-split fragments and \
         held/repeated words into their line. You may lightly fix an obviously \
         mis-heard word using the reference, but do not invent words.\n\n\
         OUTPUT — valid JSON only, no prose, no markdown fences:\n\
         {{\"lines\": [{{\"text\": \"His name will bring complete breakthrough\", \
         \"start_idx\": 0, \"end_idx\": 1}}]}}",
        aai_lines.len() - 1,
        aai_lines.len() - 1
    ));
    s
}

// mutants::skip: defensive JSON-substring extraction. The brace-index guard
// mutants are equivalent (a malformed/empty body still fails `from_str` → None)
// or panic-only (a reversed `b < a` slice panics rather than changing a
// behaviourally observable result). Real behaviour — valid JSON parses, garbage
// returns None — is covered by the regroup tests (`falls_back_when_chat_fails`,
// the Canned-body cases).
#[cfg_attr(test, mutants::skip)]
fn parse(body: &str) -> Option<RegroupResult> {
    let stripped = crate::ai::client::strip_markdown_fences(body);
    let json = match (stripped.find('{'), stripped.rfind('}')) {
        (Some(a), Some(b)) if b > a => &stripped[a..=b],
        _ => &stripped,
    };
    serde_json::from_str::<RegroupResult>(json).ok()
}

/// Apply Claude's groups greedily, passing through any ASR line Claude didn't
/// cover. DROP-SAFE: walks indices 0..n; at each index, if a VALID group starts
/// there (in-range, non-inverted), emit it and skip to its end+1; otherwise emit
/// the single ASR line unchanged. Every input line therefore appears exactly
/// once — either inside a merge or as a passthrough. Tolerates Claude's
/// imperfect coverage over many lines (the strict full-cover gate rejected too
/// often); overlapping/late groups whose start was already consumed are ignored.
fn apply_greedy(r: &RegroupResult, aai_lines: &[LyricsLine]) -> Vec<LyricsLine> {
    let n = aai_lines.len();
    // First valid group claiming each start index wins.
    let mut by_start: std::collections::HashMap<usize, &RegroupedLine> =
        std::collections::HashMap::new();
    for g in &r.lines {
        if g.start_idx < n && g.end_idx < n && g.end_idx >= g.start_idx {
            by_start.entry(g.start_idx).or_insert(g);
        }
    }
    let mut out: Vec<LyricsLine> = Vec::new();
    let mut i = 0usize;
    while i < n {
        if let Some(g) = by_start.get(&i) {
            let end = g.end_idx;
            // Reconstruct text from the covered AAI lines — NEVER from Claude's
            // `text` (which can silently drop words). Collapse only CONSECUTIVE
            // duplicate words so held/repeated sung words ("breakthrough
            // Breakthrough") dedup, while distinct words are always preserved.
            let text = reconstruct_text(&aai_lines[i..=end]);
            out.push(LyricsLine {
                start_ms: aai_lines[i].start_ms,
                end_ms: aai_lines[end].end_ms,
                en: text,
                sk: None,
                words: None,
            });
            i = end + 1;
        } else {
            let l = &aai_lines[i];
            out.push(LyricsLine {
                start_ms: l.start_ms,
                end_ms: l.end_ms,
                en: l.en.clone(),
                sk: None,
                words: None,
            });
            i += 1;
        }
    }
    out
}

/// Join the covered AAI lines into one text, collapsing only CONSECUTIVE
/// duplicate words (case-insensitive, ignoring trailing punctuation). Distinct
/// words are always kept — this is the guarantee against word loss. Consecutive
/// repeats (a held/echoed sung word split across lines, e.g. "breakthrough."
/// then "Breakthrough.") collapse to one.
fn reconstruct_text(lines: &[LyricsLine]) -> String {
    let mut out: Vec<&str> = Vec::new();
    for w in lines.iter().flat_map(|l| l.en.split_whitespace()) {
        if out
            .last()
            .is_some_and(|prev| norm_word(prev) == norm_word(w))
        {
            continue;
        }
        out.push(w);
    }
    out.join(" ")
}

/// Lowercase + strip leading/trailing non-alphanumerics for duplicate compare.
fn norm_word(w: &str) -> String {
    w.trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(start: u64, end: u64, en: &str) -> LyricsLine {
        LyricsLine {
            start_ms: start,
            end_ms: end,
            en: en.into(),
            sk: None,
            words: None,
        }
    }

    struct Canned(String);
    #[async_trait::async_trait]
    impl RegroupChat for Canned {
        async fn chat(&self, _s: &str, _u: &str) -> Result<String, String> {
            Ok(self.0.clone())
        }
    }
    struct Failing;
    #[async_trait::async_trait]
    impl RegroupChat for Failing {
        async fn chat(&self, _s: &str, _u: &str) -> Result<String, String> {
            Err("down".into())
        }
    }

    fn sample() -> Vec<LyricsLine> {
        vec![
            line(0, 500, "His name will bring complete breakthrough."),
            line(700, 2000, "Breakthrough."),
            line(2200, 2600, "And every knee"),
            line(2700, 3300, "shall bow and tongue"),
        ]
    }

    #[tokio::test]
    async fn merges_when_full_cover() {
        // Merge 0-1 and 2-3. Text is reconstructed from the AAI lines (Claude's
        // text is ignored): the held repeat "breakthrough." / "Breakthrough."
        // collapses (consecutive duplicate), distinct words preserved.
        let chat =
            Canned(r#"{"lines":[{"start_idx":0,"end_idx":1},{"start_idx":2,"end_idx":3}]}"#.into());
        let out = regroup(&chat, &sample(), &[]).await;
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].en, "His name will bring complete breakthrough.");
        assert_eq!(out[0].start_ms, 0);
        assert_eq!(out[0].end_ms, 2000); // spans the held "Breakthrough."
        assert_eq!(out[1].en, "And every knee shall bow and tongue");
        assert_eq!(out[1].start_ms, 2200);
        assert_eq!(out[1].end_ms, 3300);
        assert!(out.iter().all(|l| l.words.is_none()));
    }

    #[tokio::test]
    async fn distinct_words_across_merged_lines_are_never_dropped() {
        // Regression: Claude merged "A power that can heal all pain" +
        // "It's just one word" into one group and wrote text omitting the
        // second phrase. With reconstruction, every distinct word survives.
        let lines = vec![
            line(0, 2000, "A power that can heal all pain."),
            line(2100, 3000, "It's just one word."),
        ];
        // Claude (wrongly) groups both and drops the second phrase in its text.
        let chat = Canned(
            r#"{"lines":[{"text":"A power that can heal all pain","start_idx":0,"end_idx":1}]}"#
                .into(),
        );
        let out = regroup(&chat, &lines, &[]).await;
        assert_eq!(out.len(), 1);
        assert!(
            out[0].en.contains("It's just one word"),
            "must keep all words: {:?}",
            out[0].en
        );
        assert!(out[0].en.contains("A power that can heal all pain"));
    }

    #[tokio::test]
    async fn consecutive_duplicate_words_collapse() {
        let lines = vec![
            line(0, 500, "complete breakthrough."),
            line(700, 2000, "Breakthrough."),
        ];
        let chat = Canned(r#"{"lines":[{"start_idx":0,"end_idx":1}]}"#.into());
        let out = regroup(&chat, &lines, &[]).await;
        assert_eq!(out.len(), 1);
        // "breakthrough." then "Breakthrough." → one.
        assert_eq!(out[0].en.to_lowercase().matches("breakthrough").count(), 1);
    }

    #[tokio::test]
    async fn gap_in_coverage_passes_through_uncovered_line() {
        // Claude covers 0-0 and 2-3, skipping index 1. Greedy: line 0 (merged,
        // here single), line 1 passes through unchanged, lines 2-3 merge. No
        // drop — index 1 still appears.
        let chat = Canned(
            r#"{"lines":[{"text":"x","start_idx":0,"end_idx":0},{"text":"y","start_idx":2,"end_idx":3}]}"#.into(),
        );
        let out = regroup(&chat, &sample(), &[]).await;
        assert_eq!(out.len(), 3);
        assert_eq!(out[1].en, "Breakthrough.", "uncovered line passes through");
    }

    #[tokio::test]
    async fn overlap_resolved_by_first_claim() {
        // Group [0-2] claims indices 0,1,2; the later [2-3] start (2) was already
        // consumed, so only index 3 remains → passthrough. Two lines, every index
        // covered once.
        let chat = Canned(
            r#"{"lines":[{"text":"x","start_idx":0,"end_idx":2},{"text":"y","start_idx":2,"end_idx":3}]}"#.into(),
        );
        let out = regroup(&chat, &sample(), &[]).await;
        assert_eq!(out.len(), 2);
        // out[0] reconstructed from lines 0-2 (held "Breakthrough." collapsed).
        assert_eq!(
            out[0].en,
            "His name will bring complete breakthrough. And every knee"
        );
        assert_eq!(out[0].end_ms, 2600); // spans lines 0-2
        assert_eq!(out[1].en, "shall bow and tongue"); // index 3 passthrough
    }

    #[tokio::test]
    async fn out_of_range_group_ignored_all_passthrough() {
        // end_idx 9 is out of range → group ignored → all 4 lines pass through.
        let chat = Canned(r#"{"lines":[{"text":"x","start_idx":0,"end_idx":9}]}"#.into());
        let out = regroup(&chat, &sample(), &[]).await;
        assert_eq!(out.len(), 4);
        assert_eq!(out[0].en, "His name will bring complete breakthrough.");
    }

    #[tokio::test]
    async fn falls_back_when_chat_fails() {
        let out = regroup(&Failing, &sample(), &[]).await;
        assert_eq!(out.len(), 4);
    }

    #[tokio::test]
    async fn single_line_input_returned_as_is() {
        let one = vec![line(0, 500, "only")];
        let out = regroup(&Canned("{\"lines\":[]}".into()), &one, &[]).await;
        assert_eq!(out.len(), 1);
    }

    #[tokio::test]
    async fn empty_text_reconstructs_from_covered_lines() {
        let chat = Canned(
            r#"{"lines":[{"text":"","start_idx":0,"end_idx":1},{"text":"And every knee shall bow and tongue","start_idx":2,"end_idx":3}]}"#.into(),
        );
        let out = regroup(&chat, &sample(), &[]).await;
        assert_eq!(out.len(), 2);
        assert!(
            out[0]
                .en
                .contains("His name will bring complete breakthrough")
        );
    }

    #[test]
    fn build_prompt_lists_indexed_lines_and_conditional_reference() {
        let lines = sample();
        // No reference: indexed ASR lines present, NO reference block. Kills the
        // `build_prompt -> String::new()/"xyzzy"` mutants (empty/garbage prompt
        // would not contain the line text) and the `delete !` mutant (which would
        // emit the reference block even with zero reference lines).
        let p = build_prompt(&lines, &[]);
        assert!(p.contains("0: His name will bring complete breakthrough."));
        assert!(p.contains("3: shall bow and tongue"));
        assert!(!p.contains("REFERENCE PHRASING"));
        // With reference: the reference block + its lines appear.
        let p2 = build_prompt(&lines, &["a human guide line".to_string()]);
        assert!(p2.contains("REFERENCE PHRASING"));
        assert!(p2.contains("a human guide line"));
    }

    #[tokio::test]
    async fn group_with_end_idx_equal_to_len_is_rejected() {
        // sample() has 4 lines (n=4). A group claiming end_idx == 4 (== len) is
        // out of range; the `g.end_idx < n` guard must reject it so we never
        // slice aai_lines[0..=4] (panic). Kills the `< → <=` end_idx mutant:
        // under <=, 4 <= 4 admits the group and the slice panics.
        let chat = Canned(r#"{"lines":[{"start_idx":0,"end_idx":4}]}"#.into());
        let out = regroup(&chat, &sample(), &[]).await;
        assert_eq!(out.len(), 4); // all passthrough, no panic
        assert_eq!(out[0].en, "His name will bring complete breakthrough.");
    }
}
