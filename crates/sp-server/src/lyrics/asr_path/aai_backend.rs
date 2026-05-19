//! AssemblyAI Universal-3 Pro response types + JSON parser.
//!
//! HTTP transport lives below in Task 4; this file is types + the
//! `parse_completed_response` function that turns AAI's JSON body into
//! the safe Rust `AaiTranscript`.

use serde::Deserialize;
use std::path::Path;
use std::time::Duration;

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
    let raw: AaiTranscriptResponse = serde_json::from_str(body).map_err(AaiError::Parse)?;
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

// ---------------------------------------------------------------------------
// HTTP client
// ---------------------------------------------------------------------------

const API_BASE_DEFAULT: &str = "https://api.assemblyai.com/v2";
const SPEECH_MODEL: &str = "universal-3-pro";
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const POLL_TIMEOUT_S: u64 = 1800; // 30 min, same as eval Python
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(60);

pub struct AaiBackend {
    api_base: String,
    api_key: String,
    http: reqwest::Client,
}

impl AaiBackend {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_base: API_BASE_DEFAULT.to_string(),
            api_key: api_key.into(),
            http: reqwest::Client::new(),
        }
    }

    #[cfg(test)]
    pub fn with_base_url(api_key: impl Into<String>, api_base: impl Into<String>) -> Self {
        Self {
            api_base: api_base.into(),
            api_key: api_key.into(),
            http: reqwest::Client::new(),
        }
    }

    pub async fn transcribe(&self, audio_path: &Path) -> Result<AaiTranscript, AaiError> {
        let bytes = tokio::fs::read(audio_path)
            .await
            .map_err(|e| AaiError::Http(format!("read {audio_path:?}: {e}")))?;
        let upload_url = self.upload(bytes).await?;
        let transcript_id = self.create_transcript(&upload_url).await?;
        self.poll_until_done(&transcript_id).await
    }

    async fn upload(&self, bytes: Vec<u8>) -> Result<String, AaiError> {
        let resp = self
            .http
            .post(format!("{}/upload", self.api_base))
            .header("authorization", &self.api_key)
            .timeout(UPLOAD_TIMEOUT)
            .body(bytes)
            .send()
            .await
            .map_err(|e| AaiError::Http(format!("upload send: {e}")))?;
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(AaiError::QuotaExhausted);
        }
        let resp = resp
            .error_for_status()
            .map_err(|e| AaiError::Http(format!("upload status: {e}")))?;
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AaiError::Http(format!("upload json: {e}")))?;
        body.get("upload_url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| AaiError::Http("upload response missing upload_url".into()))
    }

    async fn create_transcript(&self, audio_url: &str) -> Result<String, AaiError> {
        let body = serde_json::json!({
            "audio_url": audio_url,
            "speech_models": [SPEECH_MODEL],
            "punctuate": true,
            "format_text": true,
            "speaker_labels": false,
            "language_detection": true,
        });
        let resp = self
            .http
            .post(format!("{}/transcript", self.api_base))
            .header("authorization", &self.api_key)
            .timeout(CONTROL_TIMEOUT)
            .json(&body)
            .send()
            .await
            .map_err(|e| AaiError::Http(format!("create send: {e}")))?;
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(AaiError::QuotaExhausted);
        }
        let resp = resp
            .error_for_status()
            .map_err(|e| AaiError::Http(format!("create status: {e}")))?;
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AaiError::Http(format!("create json: {e}")))?;
        body.get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| AaiError::Http("create response missing id".into()))
    }

    async fn poll_until_done(&self, transcript_id: &str) -> Result<AaiTranscript, AaiError> {
        let deadline = std::time::Instant::now() + Duration::from_secs(POLL_TIMEOUT_S);
        let url = format!("{}/transcript/{transcript_id}", self.api_base);
        loop {
            if std::time::Instant::now() >= deadline {
                return Err(AaiError::Timeout(POLL_TIMEOUT_S));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
            let resp = self
                .http
                .get(&url)
                .header("authorization", &self.api_key)
                .timeout(CONTROL_TIMEOUT)
                .send()
                .await
                .map_err(|e| AaiError::Http(format!("poll send: {e}")))?;
            if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                return Err(AaiError::QuotaExhausted);
            }
            let resp = resp
                .error_for_status()
                .map_err(|e| AaiError::Http(format!("poll status: {e}")))?;
            let text = resp
                .text()
                .await
                .map_err(|e| AaiError::Http(format!("poll body: {e}")))?;
            let raw: serde_json::Value = serde_json::from_str(&text).map_err(AaiError::Parse)?;
            let status = raw
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            if status == "completed" || status == "error" {
                return parse_completed_response(&text);
            }
            // else: still processing — keep polling
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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

    #[tokio::test]
    async fn transcribe_three_step_flow_happy_path() {
        let server = MockServer::start().await;

        // 1) upload returns upload_url
        Mock::given(method("POST"))
            .and(path("/upload"))
            .and(header("authorization", "test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "upload_url": "https://cdn.example/audio.wav"
            })))
            .mount(&server)
            .await;

        // 2) create transcript returns id
        Mock::given(method("POST"))
            .and(path("/transcript"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "trans-123"
            })))
            .mount(&server)
            .await;

        // 3) poll returns completed directly (no processing state)
        Mock::given(method("GET"))
            .and(path("/transcript/trans-123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "completed",
                "text": "hello world",
                "words": [
                    {"text": "hello", "start": 0, "end": 500, "confidence": 0.9},
                    {"text": "world", "start": 600, "end": 1100, "confidence": 0.9}
                ]
            })))
            .mount(&server)
            .await;

        let tmp = std::env::temp_dir().join("asr_path_test_audio.wav");
        let mut f = std::fs::File::create(&tmp).unwrap();
        f.write_all(b"\x00\x00").unwrap();
        drop(f);

        let backend = AaiBackend::with_base_url("test-key", server.uri());
        let t = backend.transcribe(&tmp).await.expect("must transcribe");
        assert_eq!(t.words.len(), 2);
        assert_eq!(t.words[0].text, "hello");
        assert_eq!(t.words[1].end_ms, 1100);

        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn transcribe_propagates_quota_exhausted() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/upload"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let tmp = std::env::temp_dir().join("asr_path_test_429.wav");
        std::fs::write(&tmp, b"\x00").unwrap();

        let backend = AaiBackend::with_base_url("test-key", server.uri());
        let err = backend.transcribe(&tmp).await.expect_err("must err");
        assert!(matches!(err, AaiError::QuotaExhausted), "got {err:?}");

        let _ = std::fs::remove_file(&tmp);
    }
}
