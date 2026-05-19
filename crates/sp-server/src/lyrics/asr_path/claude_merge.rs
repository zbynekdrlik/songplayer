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
/// runs strict deserialization. Any `start_ms` or `end_ms` field present in
/// the response triggers `MergeError::HasMsFields` BEFORE serde so the error
/// message clearly says "ms fields banned" rather than serde's generic
/// "unknown field" message. Other unknown fields trigger `MergeError::Parse`.
pub fn parse_response(body: &str) -> Result<ClaudeMergeResult, MergeError> {
    let stripped = strip_json_fence(body);
    if stripped.contains("\"start_ms\"") || stripped.contains("\"end_ms\"") {
        return Err(MergeError::HasMsFields);
    }
    serde_json::from_str(stripped).map_err(MergeError::Parse)
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
}
