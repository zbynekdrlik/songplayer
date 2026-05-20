//! Claude merge layer: parse Claude's JSON response into ClaudeMergeResult.
//!
//! Schema is strict (`deny_unknown_fields`) so a Claude response that contains
//! `start_ms` / `end_ms` fields FAILS deserialization. This is the spec-level
//! guard preventing the v15 LLM-can't-emit-exact-length-arrays disaster from
//! sneaking back in.

use serde::Deserialize;

/// One merged line. Extra fields Claude may include (e.g. stray
/// `start_time_ms` / `end_time_ms` it sometimes echoes) are IGNORED — there is
/// NO `deny_unknown_fields`. The v15 lesson ("never TRUST LLM-emitted ms") is
/// honored structurally: the resolver only ever reads the three fields below
/// and looks ms up from the AAI words by index. Claude's ms, if present, are
/// never read, so ignoring them is safe and far more robust than rejecting the
/// whole song on a habitual extra field.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MergedLine {
    pub text: String,
    pub start_word_idx: usize,
    pub end_word_idx: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClaudeMergeResult {
    #[serde(default)]
    pub disagreement: bool,
    #[serde(default)]
    pub notes: String,
    /// Accept either `lines` (the asked-for key) or `segments` (a key cloaked
    /// Claude sometimes substitutes). Extra top-level fields are ignored.
    #[serde(default, alias = "segments")]
    pub lines: Vec<MergedLine>,
}

#[derive(Debug, thiserror::Error)]
pub enum MergeError {
    #[error("Claude response parse failed: {0}")]
    Parse(serde_json::Error),
    #[error("Claude transport failed: {0}")]
    Transport(String),
}

/// Parse a raw Claude response body into a ClaudeMergeResult.
///
/// Tolerant by design (CLIProxyAPI cloaked Claude is unreliable about exact
/// shape): strips markdown fences + surrounding prose, accepts `lines` or
/// `segments`, and ignores any extra fields. The resolver reads only word
/// indices, so stray ms fields are harmless.
pub fn parse_response(body: &str) -> Result<ClaudeMergeResult, MergeError> {
    let stripped = extract_json_object(body);
    serde_json::from_str(&stripped).map_err(MergeError::Parse)
}

/// Strip markdown fences and any surrounding prose, returning the substring
/// from the first `{` to the last `}` (inclusive). Claude via CLIProxyAPI
/// frequently wraps JSON in ```fences``` or prefixes a sentence of prose
/// despite "Output JSON only"; isolating the outermost object survives both.
fn extract_json_object(body: &str) -> String {
    let unfenced = crate::ai::client::strip_markdown_fences(body);
    match (unfenced.find('{'), unfenced.rfind('}')) {
        (Some(start), Some(end)) if end > start => unfenced[start..=end].to_string(),
        _ => unfenced,
    }
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
            match parse_response(&second) {
                Ok(r) => Ok(r),
                Err(e) => {
                    // Both attempts unparseable. Log the raw second response so
                    // an operator can see EXACTLY what Claude emitted (malformed
                    // syntax, unescaped quote, truncation, prose, etc.) — mirrors
                    // translator.rs diagnostic logging. Bounded at 4000 chars.
                    let snippet: String = second.chars().take(4000).collect();
                    let truncated = second.chars().count() > 4000;
                    tracing::warn!(
                        error = %e,
                        response_len = second.chars().count(),
                        truncated,
                        response = %snippet,
                        "asr_path claude_merge: both attempts unparseable — raw response logged for diagnosis"
                    );
                    Err(e)
                }
            }
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
    fn extracts_json_object_from_surrounding_prose() {
        // CLIProxyAPI Claude often prefixes a sentence of prose before the
        // JSON despite "Output JSON only". The first-`{`-to-last-`}` extraction
        // isolates the object.
        let body = "Here is the merged result you asked for:\n\
                    {\"disagreement\": false, \"notes\": \"ok\", \"lines\": \
                    [{\"text\": \"Hi\", \"start_word_idx\": 0, \"end_word_idx\": 0}]}\n\
                    Let me know if you need anything else.";
        let r = parse_response(body).expect("must parse despite prose");
        assert!(!r.disagreement);
        assert_eq!(r.lines.len(), 1);
        assert_eq!(r.lines[0].text, "Hi");
    }

    #[test]
    fn ignores_ms_fields_on_line() {
        // Cloaked Claude habitually echoes ms even when not asked. The parser
        // IGNORES them (no deny_unknown_fields); the v15 guarantee ("never use
        // LLM ms") holds because the resolver only reads word indices and looks
        // ms up from the AAI words. Parsing must succeed, keeping the 3 fields.
        let body = r#"{
            "disagreement": false,
            "notes": "",
            "lines": [
                {"text": "Hello", "start_word_idx": 0, "end_word_idx": 1, "start_time_ms": 0, "end_time_ms": 500}
            ]
        }"#;
        let r = parse_response(body).expect("must parse, ignoring ms fields");
        assert_eq!(r.lines.len(), 1);
        assert_eq!(r.lines[0].text, "Hello");
        assert_eq!(r.lines[0].start_word_idx, 0);
        assert_eq!(r.lines[0].end_word_idx, 1);
    }

    #[test]
    fn accepts_segments_key_alias() {
        // Cloaked Claude sometimes uses `segments` instead of `lines` (observed
        // on the first production song). The alias accepts both.
        let body = r#"{
            "disagreement": false,
            "segments": [
                {"text": "Hello", "start_word_idx": 0, "end_word_idx": 1}
            ]
        }"#;
        let r = parse_response(body).expect("must parse via segments alias");
        assert_eq!(r.lines.len(), 1);
        assert_eq!(r.lines[0].text, "Hello");
    }

    #[test]
    fn parses_disagreement_with_empty_lines() {
        let body = r#"{"disagreement": true, "notes": "wrong version", "lines": []}"#;
        let r = parse_response(body).expect("ok");
        assert!(r.disagreement);
        assert!(r.lines.is_empty());
    }

    #[test]
    fn ignores_unknown_top_level_field() {
        // Extra top-level fields (e.g. Claude's "explanation") are ignored.
        let body = r#"{"disagreement": false, "notes": "", "lines": [], "extra": "x"}"#;
        let r = parse_response(body).expect("must parse, ignoring extra field");
        assert!(!r.disagreement);
        assert!(r.lines.is_empty());
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
    async fn merge_ignores_ms_fields_and_succeeds() {
        // Claude echoes ms fields out of habit. The parser ignores them and the
        // merge succeeds with the 3 real fields — no fallback, no retry.
        let (words, source, text) = ctx();
        let input = ClaudeMergeInput {
            aai_words: &words,
            untimed_text: text,
            untimed_source: source,
            language: None,
        };
        let chat = ScriptedChat::new(vec![
            r#"{"disagreement": false, "notes": "", "lines": [{"text": "Hi", "start_word_idx": 0, "end_word_idx": 0, "start_time_ms": 0, "end_time_ms": 200}]}"#,
        ]);
        let r = merge(&chat, &input).await.expect("must merge, ignoring ms");
        assert_eq!(r.lines.len(), 1);
        assert_eq!(r.lines[0].text, "Hi");
        assert_eq!(r.lines[0].end_word_idx, 0);
    }
}
