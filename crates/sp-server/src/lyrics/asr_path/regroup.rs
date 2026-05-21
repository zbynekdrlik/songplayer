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
    async fn chat(&self, system: &str, user: &str) -> Result<String, String> {
        crate::ai::client::AiClient::chat(self, system, user)
            .await
            .map_err(|e| e.to_string())
    }
}

#[derive(Debug, Clone, Deserialize)]
struct RegroupedLine {
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
        Some(groups) if is_full_cover(&groups, aai_lines.len()) => apply(&groups, aai_lines),
        _ => {
            tracing::warn!(
                "asr_path regroup: Claude grouping invalid/incomplete — using raw split"
            );
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

fn parse(body: &str) -> Option<RegroupResult> {
    let stripped = crate::ai::client::strip_markdown_fences(body);
    let json = match (stripped.find('{'), stripped.rfind('}')) {
        (Some(a), Some(b)) if b > a => &stripped[a..=b],
        _ => &stripped,
    };
    serde_json::from_str::<RegroupResult>(json).ok()
}

/// True iff the groups are strictly forward and cover [0, len) exactly once.
fn is_full_cover(r: &RegroupResult, len: usize) -> bool {
    if r.lines.is_empty() {
        return false;
    }
    let mut expected = 0usize;
    for g in &r.lines {
        if g.start_idx != expected || g.end_idx < g.start_idx || g.end_idx >= len {
            return false;
        }
        expected = g.end_idx + 1;
    }
    expected == len
}

fn apply(r: &RegroupResult, aai_lines: &[LyricsLine]) -> Vec<LyricsLine> {
    r.lines
        .iter()
        .map(|g| {
            let text = if g.text.trim().is_empty() {
                // Defensive: if Claude emitted empty text, reconstruct from the
                // covered ASR lines so we never show a blank line.
                aai_lines[g.start_idx..=g.end_idx]
                    .iter()
                    .map(|l| l.en.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                g.text.trim().to_string()
            };
            LyricsLine {
                start_ms: aai_lines[g.start_idx].start_ms,
                end_ms: aai_lines[g.end_idx].end_ms,
                en: text,
                sk: None,
                words: None,
            }
        })
        .collect()
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
        // Merge 0-1 ("breakthrough" + held repeat) and 2-3 ("and every knee...").
        let chat = Canned(
            r#"{"lines":[{"text":"His name will bring complete breakthrough","start_idx":0,"end_idx":1},{"text":"And every knee shall bow and tongue","start_idx":2,"end_idx":3}]}"#.into(),
        );
        let out = regroup(&chat, &sample(), &[]).await;
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].en, "His name will bring complete breakthrough");
        assert_eq!(out[0].start_ms, 0);
        assert_eq!(out[0].end_ms, 2000); // spans the held "Breakthrough."
        assert_eq!(out[1].start_ms, 2200);
        assert_eq!(out[1].end_ms, 3300);
        assert!(out.iter().all(|l| l.words.is_none()));
    }

    #[tokio::test]
    async fn falls_back_when_gap_in_coverage() {
        // Skips index 1 → not full cover → return raw 4 lines unchanged.
        let chat = Canned(
            r#"{"lines":[{"text":"x","start_idx":0,"end_idx":0},{"text":"y","start_idx":2,"end_idx":3}]}"#.into(),
        );
        let out = regroup(&chat, &sample(), &[]).await;
        assert_eq!(out.len(), 4, "incomplete coverage must fall back to raw");
        assert_eq!(out[1].en, "Breakthrough.");
    }

    #[tokio::test]
    async fn falls_back_when_overlap() {
        let chat = Canned(
            r#"{"lines":[{"text":"x","start_idx":0,"end_idx":2},{"text":"y","start_idx":2,"end_idx":3}]}"#.into(),
        );
        let out = regroup(&chat, &sample(), &[]).await;
        assert_eq!(out.len(), 4, "overlap must fall back");
    }

    #[tokio::test]
    async fn falls_back_when_out_of_range() {
        let chat = Canned(r#"{"lines":[{"text":"x","start_idx":0,"end_idx":9}]}"#.into());
        let out = regroup(&chat, &sample(), &[]).await;
        assert_eq!(out.len(), 4);
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
}
