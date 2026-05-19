//! Claude merge layer: parse Claude's JSON response into ClaudeMergeResult.
//!
//! Schema is strict (`deny_unknown_fields`) so a Claude response that contains
//! `start_ms` / `end_ms` fields FAILS deserialization. This is the spec-level
//! guard preventing the v15 LLM-can't-emit-exact-length-arrays disaster from
//! sneaking back in.

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergedLine {
    pub text: String,
    pub start_word_idx: usize,
    pub end_word_idx: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeMergeResult {
    pub disagreement: bool,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub lines: Vec<MergedLine>,
}

#[derive(Debug, thiserror::Error)]
pub enum MergeError {
    #[error("Claude response parse failed: {0}")]
    Parse(serde_json::Error),
    #[error("Claude response contained ms fields (banned per spec)")]
    HasMsFields,
    #[error("Claude transport failed: {0}")]
    Transport(String),
}

/// Parse a raw Claude response body into a ClaudeMergeResult.
///
/// Strips a leading ```json ... ``` fence if present (Claude sometimes wraps
/// JSON output in markdown despite "Output JSON only" instructions). Then
/// runs strict deserialization. Any `start_ms` or `end_ms` key present in a
/// `lines[]` object triggers `MergeError::HasMsFields` — the check is
/// intentionally narrow (lines only) so Claude mentioning "start_ms" in the
/// `notes` field does NOT cause a false positive. Other unknown fields trigger
/// `MergeError::Parse`.
pub fn parse_response(body: &str) -> Result<ClaudeMergeResult, MergeError> {
    let stripped = strip_json_fence(body);
    // Parse to Value first so the ms-field check can walk the structure
    // precisely (lines[] only — the spec ban only applies there).
    let value: serde_json::Value = serde_json::from_str(stripped).map_err(MergeError::Parse)?;
    if let Some(lines) = value.get("lines").and_then(|v| v.as_array()) {
        for line in lines {
            if let Some(obj) = line.as_object() {
                if obj.contains_key("start_ms") || obj.contains_key("end_ms") {
                    return Err(MergeError::HasMsFields);
                }
            }
        }
    }
    serde_json::from_value(value).map_err(MergeError::Parse)
}

fn strip_json_fence(body: &str) -> &str {
    let t = body.trim();
    if let Some(rest) = t.strip_prefix("```json") {
        let rest = rest.trim_start_matches('\n');
        if let Some(inner) = rest.strip_suffix("```") {
            return inner.trim();
        }
    }
    if let Some(rest) = t.strip_prefix("```") {
        let rest = rest.trim_start_matches('\n');
        if let Some(inner) = rest.strip_suffix("```") {
            return inner.trim();
        }
    }
    t
}

use crate::lyrics::asr_path::merge_prompt::{ClaudeMergeInput, SYSTEM_PROMPT, build_user_prompt};

/// Abstraction over `AiClient::chat` so tests can inject canned responses
/// without hitting CLIProxyAPI. Implemented for `crate::ai::client::AiClient`
/// at the bottom of this file.
#[async_trait::async_trait]
pub trait MergeChat: Send + Sync {
    async fn chat(&self, system: &str, user: &str) -> Result<String, String>;
}

/// Build prompt → call Claude → parse → return ClaudeMergeResult.
///
/// Single retry on `MergeError::Parse`. On retry the user prompt is appended
/// with a stricter directive: CLIProxyAPI occasionally wraps JSON in markdown
/// fences or prefixes prose despite the system rules; the reinforced instruction
/// usually recovers.
pub async fn merge<C: MergeChat + ?Sized>(
    chat: &C,
    input: &ClaudeMergeInput<'_>,
) -> Result<ClaudeMergeResult, MergeError> {
    let user = build_user_prompt(input);
    let first = chat
        .chat(SYSTEM_PROMPT, &user)
        .await
        .map_err(MergeError::Transport)?;
    match parse_response(&first) {
        Ok(r) => Ok(r),
        Err(MergeError::Parse(_)) => {
            // Append a stricter directive on retry; CLIProxyAPI occasionally
            // wraps JSON in markdown fences or prefixes prose despite the
            // system rules. The reinforced instruction usually recovers.
            let stricter = format!(
                "{user}\n\nIMPORTANT: Previous response was unparseable JSON. \
                 Output ONLY valid JSON matching the schema. No markdown fences, \
                 no prose, no explanation."
            );
            let second = chat
                .chat(SYSTEM_PROMPT, &stricter)
                .await
                .map_err(MergeError::Transport)?;
            parse_response(&second)
        }
        Err(e) => Err(e),
    }
}

#[async_trait::async_trait]
impl MergeChat for crate::ai::client::AiClient {
    // Trait delegation to the real CLIProxyAPI client. Mutations on this
    // body (e.g. replacing the call with `Ok(String::new())`) are unkillable
    // in unit tests because the real path requires live HTTP to CLIProxyAPI.
    // Behavioral coverage comes from the `ScriptedChat`-based tests above,
    // which exercise all `merge()` paths using the trait abstraction.
    #[cfg_attr(test, mutants::skip)]
    async fn chat(&self, system: &str, user: &str) -> Result<String, String> {
        crate::ai::client::AiClient::chat(self, system, user)
            .await
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_formed_response() {
        let body = r#"{
            "disagreement": false,
            "notes": "matched 95% of source",
            "lines": [
                {"text": "Hello world", "start_word_idx": 0, "end_word_idx": 1},
                {"text": "How are you", "start_word_idx": 2, "end_word_idx": 4}
            ]
        }"#;
        let r = parse_response(body).expect("ok");
        assert!(!r.disagreement);
        assert_eq!(r.lines.len(), 2);
        assert_eq!(r.lines[0].start_word_idx, 0);
        assert_eq!(r.lines[1].end_word_idx, 4);
    }

    #[test]
    fn strips_markdown_fence() {
        let body = "```json\n{\"disagreement\": false, \"notes\": \"\", \"lines\": []}\n```";
        let r = parse_response(body).expect("ok");
        assert!(r.lines.is_empty());
    }

    #[test]
    fn rejects_ms_field_on_line() {
        // The v15-prevention guard. If Claude emits ms values, parse_response
        // returns HasMsFields, and the orchestrator falls back to raw AAI.
        let body = r#"{
            "disagreement": false,
            "notes": "",
            "lines": [
                {"text": "Hello", "start_word_idx": 0, "end_word_idx": 1, "start_ms": 0, "end_ms": 500}
            ]
        }"#;
        let err = parse_response(body).expect_err("must err");
        assert!(matches!(err, MergeError::HasMsFields), "got {err:?}");
    }

    #[test]
    fn parses_disagreement_with_empty_lines() {
        let body = r#"{"disagreement": true, "notes": "wrong version", "lines": []}"#;
        let r = parse_response(body).expect("ok");
        assert!(r.disagreement);
        assert!(r.lines.is_empty());
    }

    #[test]
    fn rejects_unknown_top_level_field() {
        let body = r#"{"disagreement": false, "notes": "", "lines": [], "extra": "x"}"#;
        let err = parse_response(body).expect_err("must err");
        // serde produces Parse, not HasMsFields, for non-ms unknown fields.
        assert!(matches!(err, MergeError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn ms_keyword_in_notes_does_not_trigger_has_ms_fields() {
        // Claude can mention 'start_ms' in the notes field (e.g. as part of
        // an explanation). That must NOT be confused with actual ms-valued
        // line fields (the v15-prevention rule applies to lines[*].start_ms /
        // end_ms only).
        let body = r#"{
            "disagreement": false,
            "notes": "I ignored start_ms in input — used word indices only",
            "lines": [
                {"text": "Hello", "start_word_idx": 0, "end_word_idx": 0}
            ]
        }"#;
        let r = parse_response(body).expect("must parse");
        assert!(!r.disagreement);
        assert_eq!(r.lines.len(), 1);
    }

    use crate::lyrics::asr_path::aai_backend::AaiWord;
    use crate::lyrics::asr_path::merge_prompt::ClaudeMergeInput;

    struct ScriptedChat {
        responses: std::sync::Mutex<Vec<String>>,
    }

    impl ScriptedChat {
        fn new(responses: Vec<&str>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses.into_iter().map(String::from).collect()),
            }
        }
    }

    #[async_trait::async_trait]
    impl MergeChat for ScriptedChat {
        async fn chat(&self, _system: &str, _user: &str) -> Result<String, String> {
            self.responses
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| "out of scripted responses".to_string())
        }
    }

    fn ctx() -> (Vec<AaiWord>, &'static str, &'static str) {
        let words = vec![AaiWord {
            text: "hi".to_string(),
            start_ms: 0,
            end_ms: 200,
            confidence: 0.9,
        }];
        (words, "genius", "hi")
    }

    #[tokio::test]
    async fn merge_returns_parsed_response() {
        let (words, source, text) = ctx();
        let input = ClaudeMergeInput {
            aai_words: &words,
            untimed_text: text,
            untimed_source: source,
            language: None,
        };
        let chat = ScriptedChat::new(vec![
            r#"{"disagreement": false, "notes": "", "lines": [{"text": "Hi", "start_word_idx": 0, "end_word_idx": 0}]}"#,
        ]);
        let r = merge(&chat, &input).await.expect("ok");
        assert!(!r.disagreement);
        assert_eq!(r.lines.len(), 1);
    }

    #[tokio::test]
    async fn merge_retries_once_on_parse_error_then_succeeds() {
        let (words, source, text) = ctx();
        let input = ClaudeMergeInput {
            aai_words: &words,
            untimed_text: text,
            untimed_source: source,
            language: None,
        };
        // ScriptedChat::pop pops from END (Vec::pop is LIFO). We want:
        //   1st call (popped first) → bad/malformed → triggers Parse error
        //   2nd call (popped second / retry) → good → succeeds
        // So push GOOD first, BAD last so BAD is popped first.
        let chat = ScriptedChat::new(vec![
            r#"{"disagreement": false, "notes": "", "lines": []}"#, // popped second (retry result)
            r#"not valid json"#,                                    // popped first (initial result)
        ]);
        let r = merge(&chat, &input).await.expect("ok");
        assert!(r.lines.is_empty());
    }

    #[tokio::test]
    async fn merge_propagates_ms_field_error() {
        let (words, source, text) = ctx();
        let input = ClaudeMergeInput {
            aai_words: &words,
            untimed_text: text,
            untimed_source: source,
            language: None,
        };
        let chat = ScriptedChat::new(vec![
            r#"{"disagreement": false, "notes": "", "lines": [{"text": "x", "start_word_idx": 0, "end_word_idx": 0, "start_ms": 0}]}"#,
        ]);
        let err = merge(&chat, &input).await.expect_err("must err");
        // HasMsFields is NOT MergeError::Parse, so no retry happens.
        assert!(matches!(err, MergeError::HasMsFields), "got {err:?}");
    }
}
