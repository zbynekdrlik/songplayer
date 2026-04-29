//! Replicate API client — explicit upload-then-predict path with
//! rate-limit-aware spacing + 429 backoff.
//!
//! WHY explicit upload-then-predict: during verification, `client.run()`
//! returned 404s on file inputs (replicate Python lib v1.0.7 issue).
//! Direct API calls work reliably:
//!   1. POST /v1/files (multipart) → URL
//!   2. POST /v1/predictions (model+version+input{audio_file:URL}) → prediction
//!   3. GET  /v1/predictions/{id} polled until status terminal

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::time::sleep;

const REPLICATE_BASE: &str = "https://api.replicate.com/v1";
/// Burst-1 rate limit at <$5 balance: 1 request per 12s window.
const RATE_LIMIT_SPACING: Duration = Duration::from_secs(12);
const RETRY_BASE: Duration = Duration::from_secs(10);
const RETRY_CAP: Duration = Duration::from_secs(60);
const RETRY_MAX_ATTEMPTS: u32 = 4;
const POLL_INTERVAL: Duration = Duration::from_secs(8);
pub const PREDICTION_TIMEOUT: Duration = Duration::from_secs(1800);
const PER_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Error)]
pub enum ReplicateError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("replicate {status}: {body}")]
    ApiError { status: u16, body: String },
    #[error("rate-limited after {0} attempts")]
    RateLimited(u32),
    #[error("prediction failed: {0}")]
    PredictionFailed(String),
    #[error("prediction timed out")]
    Timeout,
    #[error("malformed response: {0}")]
    Malformed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredictionResponse {
    pub id: String,
    pub status: String,
    pub output: Option<Value>,
    pub error: Option<String>,
    pub metrics: Option<Value>,
}

pub struct ReplicateClient {
    api_token: String,
    http: reqwest::Client,
}

impl ReplicateClient {
    pub fn new(api_token: impl Into<String>) -> Self {
        Self {
            api_token: api_token.into(),
            http: reqwest::Client::builder()
                .timeout(PER_REQUEST_TIMEOUT)
                .build()
                .expect("reqwest client"),
        }
    }

    /// Upload a file via /v1/files. Returns the URL Replicate will fetch from.
    pub async fn upload_file(&self, path: &Path) -> Result<String, ReplicateError> {
        let bytes = tokio::fs::read(path).await?;
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("audio.wav")
            .to_string();
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(file_name)
            .mime_str("audio/wav")
            .map_err(|e| ReplicateError::Malformed(e.to_string()))?;
        let form = reqwest::multipart::Form::new().part("content", part);

        let resp = self
            .http
            .post(format!("{REPLICATE_BASE}/files"))
            .bearer_auth(&self.api_token)
            .multipart(form)
            .send()
            .await?;

        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            return Err(ReplicateError::ApiError {
                status: status.as_u16(),
                body,
            });
        }
        let v: Value = serde_json::from_str(&body)
            .map_err(|e| ReplicateError::Malformed(format!("file response: {e}")))?;
        v["urls"]["get"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| ReplicateError::Malformed("missing urls.get in file response".into()))
    }

    /// Create + poll a prediction with rate-limit spacing + 429 backoff.
    pub async fn predict(
        &self,
        version: &str,
        input: Value,
    ) -> Result<PredictionResponse, ReplicateError> {
        // 1. Burst-1 spacing (always wait 12s before creating a prediction)
        sleep(RATE_LIMIT_SPACING).await;

        // 2. Create prediction with 429 backoff
        let mut attempt = 0;
        let pred = loop {
            attempt += 1;
            let body = serde_json::json!({ "version": version, "input": input });
            let resp = self
                .http
                .post(format!("{REPLICATE_BASE}/predictions"))
                .bearer_auth(&self.api_token)
                .json(&body)
                .send()
                .await?;

            let status = resp.status();
            if status.as_u16() == 429 {
                if attempt >= RETRY_MAX_ATTEMPTS {
                    return Err(ReplicateError::RateLimited(attempt));
                }
                let backoff = (RETRY_BASE * 2_u32.pow(attempt - 1)).min(RETRY_CAP);
                sleep(backoff).await;
                continue;
            }
            let resp_body = resp.text().await?;
            if !status.is_success() {
                return Err(ReplicateError::ApiError {
                    status: status.as_u16(),
                    body: resp_body,
                });
            }
            let p: PredictionResponse = serde_json::from_str(&resp_body)
                .map_err(|e| ReplicateError::Malformed(format!("predict response: {e}")))?;
            break p;
        };

        // 3. Poll until terminal
        let started = std::time::Instant::now();
        let mut current = pred;
        loop {
            if matches!(current.status.as_str(), "succeeded" | "failed" | "canceled") {
                break;
            }
            if started.elapsed() > PREDICTION_TIMEOUT {
                return Err(ReplicateError::Timeout);
            }
            sleep(POLL_INTERVAL).await;

            let resp = self
                .http
                .get(format!("{REPLICATE_BASE}/predictions/{}", current.id))
                .bearer_auth(&self.api_token)
                .send()
                .await?;
            let status = resp.status();
            let body = resp.text().await?;
            if !status.is_success() {
                return Err(ReplicateError::ApiError {
                    status: status.as_u16(),
                    body,
                });
            }
            current = serde_json::from_str(&body)
                .map_err(|e| ReplicateError::Malformed(format!("poll response: {e}")))?;
        }

        if current.status != "succeeded" {
            return Err(ReplicateError::PredictionFailed(
                current
                    .error
                    .unwrap_or_else(|| format!("status={}", current.status)),
            ));
        }
        Ok(current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_spacing_is_12_seconds() {
        assert_eq!(RATE_LIMIT_SPACING, Duration::from_secs(12));
    }

    #[test]
    fn retry_attempts_capped_at_4() {
        assert_eq!(RETRY_MAX_ATTEMPTS, 4);
    }

    #[test]
    fn retry_backoff_caps_at_60_seconds() {
        // 10 → 20 → 40 → 60 (capped)
        for attempt in 1..=4u32 {
            let backoff = (RETRY_BASE * 2_u32.pow(attempt - 1)).min(RETRY_CAP);
            assert!(backoff >= RETRY_BASE);
            assert!(backoff <= RETRY_CAP);
        }
    }

    #[test]
    fn replicate_client_constructs_with_token() {
        let _c = ReplicateClient::new("test-token");
    }
}
