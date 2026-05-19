//! AssemblyAI Universal-3 Pro response types + JSON parser.
//!
//! HTTP transport lives below in Task 4; this file is types + the
//! `parse_completed_response` function that turns AAI's JSON body into
//! the safe Rust `AaiTranscript`.

use serde::Deserialize;

/// One ASR-emitted word with its acoustic timing.
#[derive(Debug, Clone, PartialEq)]
pub struct AaiWord {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub confidence: f32,
}

/// Result of a completed AAI transcription job.
#[derive(Debug, Clone)]
pub struct AaiTranscript {
    pub words: Vec<AaiWord>,
    pub raw_text: String,
}

/// Wire-format types — deserialize-only. AAI's `/v2/transcript/{id}`
/// response when `status == "completed"`.
#[derive(Debug, Deserialize)]
struct AaiTranscriptResponse {
    status: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    words: Vec<AaiWordResponse>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AaiWordResponse {
    text: String,
    start: u64,
    end: u64,
    #[serde(default = "default_confidence")]
    confidence: f32,
}

fn default_confidence() -> f32 {
    0.9
}

pub fn parse_completed_response(body: &str) -> Result<AaiTranscript, AaiError> {
    let raw: AaiTranscriptResponse =
        serde_json::from_str(body).map_err(AaiError::Parse)?;
    match raw.status.as_str() {
        "completed" => Ok(AaiTranscript {
            words: raw
                .words
                .into_iter()
                .map(|w| AaiWord {
                    text: w.text,
                    start_ms: w.start,
                    end_ms: w.end,
                    confidence: w.confidence,
                })
                .collect(),
            raw_text: raw.text,
        }),
        "error" => Err(AaiError::Remote(
            raw.error.unwrap_or_else(|| "unknown".to_string()),
        )),
        other => Err(AaiError::UnexpectedStatus(other.to_string())),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AaiError {
    #[error("AAI response parse failed: {0}")]
    Parse(serde_json::Error),
    #[error("AAI returned status=error: {0}")]
    Remote(String),
    #[error("AAI returned unexpected status: {0}")]
    UnexpectedStatus(String),
    #[error("AAI HTTP error: {0}")]
    Http(String),
    #[error("AAI poll timed out after {0}s")]
    Timeout(u64),
    #[error("AAI quota exhausted (HTTP 429)")]
    QuotaExhausted,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_completed_with_words() {
        let body = r#"{
            "status": "completed",
            "text": "no pulse alright",
            "words": [
                {"text": "no", "start": 0, "end": 500, "confidence": 0.91},
                {"text": "pulse", "start": 500, "end": 900, "confidence": 0.95},
                {"text": "alright", "start": 1100, "end": 1500, "confidence": 0.88}
            ]
        }"#;
        let t = parse_completed_response(body).expect("parse ok");
        assert_eq!(t.words.len(), 3);
        assert_eq!(t.words[0].text, "no");
        assert_eq!(t.words[0].start_ms, 0);
        assert_eq!(t.words[0].end_ms, 500);
        assert_eq!(t.raw_text, "no pulse alright");
    }

    #[test]
    fn parses_error_status() {
        let body = r#"{"status": "error", "error": "audio too short"}"#;
        let err = parse_completed_response(body).expect_err("must error");
        assert!(matches!(err, AaiError::Remote(ref m) if m == "audio too short"));
    }

    #[test]
    fn rejects_unexpected_status() {
        let body = r#"{"status": "processing"}"#;
        let err = parse_completed_response(body).expect_err("must error");
        assert!(matches!(err, AaiError::UnexpectedStatus(ref s) if s == "processing"));
    }

    #[test]
    fn missing_confidence_defaults() {
        let body = r#"{
            "status": "completed",
            "text": "x",
            "words": [{"text": "x", "start": 0, "end": 100}]
        }"#;
        let t = parse_completed_response(body).expect("ok");
        assert!((t.words[0].confidence - 0.9).abs() < f32::EPSILON);
    }
}
