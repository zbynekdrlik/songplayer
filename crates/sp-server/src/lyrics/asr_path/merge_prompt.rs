//! Prompt builder for the AAI → Claude line-merge step.
//!
//! Claude reads (numbered ASR words, untimed reference text) and regroups the
//! words into singable lines as word-index ranges. Design notes (hard-won on
//! the first production song, Planetshakers "The Greatest Name", video 85):
//!
//! - **Empty system prompt; everything in the user message.** Claude via
//!   CLIProxyAPI OAuth ("cloaked" Claude) frequently ignores the system role.
//!   The translator (`translator.rs`) hit the same wall and ships an empty
//!   system prompt. With the schema only in the system prompt, Claude invented
//!   its own (`{"segments": [...]}` with `start_time_ms`/`end_time_ms`) and
//!   emitted invalid JSON (missing comma) — every merge fell back.
//! - **No timestamps in the input.** The earlier prompt listed each word as
//!   `idx: "word" @ start_ms..end_ms`. Claude echoed those times back as
//!   `start_time_ms`/`end_time_ms` fields. Showing only `idx: word` removes the
//!   temptation; the server already owns the real ms (resolver looks them up by
//!   index from the AAI words). Word INDICES only — never ms — per the v15
//!   lesson.
//! - **A worked example pins the exact output shape** so Claude uses `lines`
//!   (not `segments`) and the three required fields.

use crate::lyrics::asr_path::aai_backend::AaiWord;

/// Empty — CLIProxyAPI cloaked Claude behaves best with everything in the user
/// message (mirrors `translator::translate_via_claude`, which passes `""`).
pub const SYSTEM_PROMPT: &str = "";

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
    let last_idx = input.aai_words.len().saturating_sub(1);
    let mut s = String::new();

    s.push_str(
        "I have a list of numbered words from an audio recording, plus the \
         reference text of what is being sung. Regroup the numbered words into \
         natural singable lines.\n\n",
    );

    s.push_str("NUMBERED WORDS (from the audio):\n");
    for (i, w) in input.aai_words.iter().enumerate() {
        s.push_str(&format!("{i}: {}\n", w.text));
    }

    s.push_str(&format!(
        "\nREFERENCE TEXT (source: {}",
        input.untimed_source
    ));
    if let Some(lang) = input.language {
        s.push_str(&format!(", language: {lang}"));
    }
    s.push_str(") — the correct words, may differ slightly from the audio:\n");
    s.push_str(&sanitize_untimed_text(input.untimed_text));

    s.push_str(&format!(
        "\n\nTASK:\n\
         Group the numbered words (indices 0 to {last_idx}) into lines. Each line \
         covers a contiguous range of indices. Fix obvious mis-hearings using the \
         reference text. Choose line breaks for natural singing phrases. Drop \
         ad-libs / repeated filler (\"yeah\", \"hey\") not in the reference.\n\n\
         OUTPUT — valid JSON only, exactly this shape (no markdown fences, no \
         prose):\n\
         {{\"disagreement\": false, \"notes\": \"\", \"lines\": [{{\"text\": \
         \"There is a name\", \"start_word_idx\": 0, \"end_word_idx\": 3}}]}}\n\n\
         RULES:\n\
         - Use ONLY indices 0 to {last_idx}.\n\
         - Each line object has exactly three fields: text, start_word_idx, \
         end_word_idx. No timestamps, no other fields.\n\
         - \"text\" is what those words say, corrected by the reference.\n\
         - If the reference is a totally different song, set \"disagreement\": \
         true and \"lines\": [].\n\
         - Output the JSON object and nothing else."
    ));

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
    fn system_prompt_is_empty() {
        // Cloaked CLIProxyAPI Claude ignores the system role; everything must
        // live in the user message (mirrors translator.rs).
        assert_eq!(SYSTEM_PROMPT, "");
    }

    #[test]
    fn user_prompt_mentions_word_indices_and_example() {
        let words = vec![word("hello", 0, 500)];
        let input = ClaudeMergeInput {
            aai_words: &words,
            untimed_text: "hello",
            untimed_source: "genius",
            language: None,
        };
        let s = build_user_prompt(&input);
        assert!(s.contains("start_word_idx"));
        assert!(s.contains("end_word_idx"));
        // The worked example pins the `lines` key (not `segments`).
        assert!(s.contains("\"lines\""));
    }

    #[test]
    fn user_prompt_omits_timestamps_from_word_list() {
        // The whole point of the rewrite: Claude never sees ms, so it cannot
        // echo them back as start_time_ms/end_time_ms fields.
        let words = vec![word("hello", 0, 500), word("world", 600, 1100)];
        let input = ClaudeMergeInput {
            aai_words: &words,
            untimed_text: "hello world",
            untimed_source: "genius",
            language: Some("en"),
        };
        let s = build_user_prompt(&input);
        assert!(s.contains("0: hello"));
        assert!(s.contains("1: world"));
        assert!(!s.contains("@ 0..500"), "word list must not include ms");
        assert!(!s.contains("500"), "no raw ms anywhere in the word list");
        assert!(s.contains("source: genius"));
        assert!(s.contains("language: en"));
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
        // The reference-text section must not carry raw triple-backticks; the
        // example JSON in the TASK section uses braces, not backticks.
        assert!(s.contains("'''"), "backticks should be replaced");
        assert!(!s.contains("```"), "no raw triple-backticks anywhere");
    }
}
