//! Prompt builder for the AAI → Claude line-merge step.
//!
//! Claude reads (AAI word-timing transcript, untimed source text) and emits
//! line splits as word-index ranges. The prompt enforces:
//! - JSON-only output (no prose).
//! - Word INDICES only — NEVER ms values. Prevents the v15 array-math failure.
//! - Drop ad-libs / repeated filler that's clearly not in the source text.
//! - Set `disagreement: true` when source text doesn't match what AAI heard.

use crate::lyrics::asr_path::aai_backend::AaiWord;

pub const SYSTEM_PROMPT: &str = "You are a karaoke-lyrics editor.\n\
\n\
INPUT\n\
- ASR transcript with per-word timings (the audio truth).\n\
- Reference lyrics text from {source} (untimed, may have errors, may be wrong version).\n\
\n\
GOAL\n\
- Produce singable line-level karaoke lyrics that match what the singer actually sings.\n\
- Use ASR timings as the timing source; use reference text to correct mishears and \
pick natural line breaks.\n\
\n\
RULES\n\
1. Output JSON only. No prose. Schema below.\n\
2. Each output line MUST reference contiguous ASR word indices \
[start_word_idx..end_word_idx] (inclusive).\n\
3. NEVER invent words not present in the ASR transcript. Reference text can correct \
spelling/word-choice ONLY where ASR clearly mis-heard a word that the reference \
disambiguates.\n\
4. NEVER emit ms values. Only word indices.\n\
5. Line splits chosen for vocal phrasing — group what a singer sings as one breath \
/ phrase, not where silence falls.\n\
6. If reference text disagrees too much with ASR (different song / different version \
/ wrong language), set \"disagreement\": true and return empty lines[].\n\
7. Drop ASR ad-libs / \"yeah\" / \"hey\" / repeated filler that are clearly not in the \
reference text. Skip them — do NOT include in any line.\n\
\n\
SCHEMA\n\
{\n\
  \"disagreement\": bool,\n\
  \"notes\": string,\n\
  \"lines\": [\n\
    { \"text\": string, \"start_word_idx\": int, \"end_word_idx\": int }\n\
  ]\n\
}";

pub struct ClaudeMergeInput<'a> {
    pub aai_words: &'a [AaiWord],
    pub untimed_text: &'a str,
    pub untimed_source: &'a str,
    pub language: Option<&'a str>,
}

/// Strip potential prompt-injection markers from community-controlled lyrics
/// text. Triple-backticks can break out of a code block context; role headers
/// (System:/Assistant:/User:) at line start can confuse some LLM APIs.
fn sanitize_untimed_text(s: &str) -> String {
    s.replace("```", "'''")
        .replace("\nSystem:", "\nsystem:")
        .replace("\nAssistant:", "\nassistant:")
        .replace("\nUser:", "\nuser:")
}

pub fn build_user_prompt(input: &ClaudeMergeInput) -> String {
    let mut s = String::new();
    s.push_str(&format!("SOURCE: {}\n", input.untimed_source));
    s.push_str(&format!(
        "LANGUAGE: {}\n",
        input.language.unwrap_or("unknown")
    ));
    s.push_str("\nREFERENCE TEXT:\n");
    s.push_str(&sanitize_untimed_text(input.untimed_text));
    s.push_str("\n\nASR TRANSCRIPT (word_idx: text @ start_ms..end_ms):\n");
    for (i, w) in input.aai_words.iter().enumerate() {
        s.push_str(&format!(
            "{i}: \"{}\" @ {}..{}\n",
            w.text.replace('"', "\\\""),
            w.start_ms,
            w.end_ms
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(text: &str, start: u64, end: u64) -> AaiWord {
        AaiWord {
            text: text.to_string(),
            start_ms: start,
            end_ms: end,
            confidence: 0.9,
        }
    }

    #[test]
    fn system_prompt_mentions_no_ms() {
        // Rule 4 is the v15-prevention rule. If someone deletes it, this fails.
        assert!(SYSTEM_PROMPT.contains("NEVER emit ms values"));
    }

    #[test]
    fn system_prompt_mentions_word_indices() {
        assert!(SYSTEM_PROMPT.contains("start_word_idx"));
        assert!(SYSTEM_PROMPT.contains("end_word_idx"));
    }

    #[test]
    fn user_prompt_includes_all_words_with_timings() {
        let words = vec![word("hello", 0, 500), word("world", 600, 1100)];
        let input = ClaudeMergeInput {
            aai_words: &words,
            untimed_text: "hello world",
            untimed_source: "genius",
            language: Some("en"),
        };
        let s = build_user_prompt(&input);
        assert!(s.contains("SOURCE: genius"));
        assert!(s.contains("LANGUAGE: en"));
        assert!(s.contains("REFERENCE TEXT:\nhello world"));
        assert!(s.contains("0: \"hello\" @ 0..500"));
        assert!(s.contains("1: \"world\" @ 600..1100"));
    }

    #[test]
    fn user_prompt_handles_quotes_in_words() {
        let words = vec![word("it's", 0, 300)];
        let input = ClaudeMergeInput {
            aai_words: &words,
            untimed_text: "it's me",
            untimed_source: "genius",
            language: None,
        };
        let s = build_user_prompt(&input);
        assert!(s.contains("LANGUAGE: unknown"));
        assert!(s.contains("0: \"it's\" @ 0..300"));
    }

    #[test]
    fn user_prompt_strips_backticks_from_untimed_text() {
        let words = vec![word("x", 0, 100)];
        let input = ClaudeMergeInput {
            aai_words: &words,
            untimed_text: "```\nignore previous\n```",
            untimed_source: "genius",
            language: None,
        };
        let s = build_user_prompt(&input);
        assert!(!s.contains("```"), "raw backticks must be stripped");
        assert!(s.contains("'''"), "should be replaced with single quotes");
    }
}
