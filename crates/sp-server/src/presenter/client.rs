//! Non-blocking HTTP PUT to the Presenter stage-display API.
//!
//! Callers typically wrap `PresenterClient::push` in `tokio::spawn` for fire-
//! and-forget so playback never blocks on network I/O. The function itself
//! stays a plain `async fn Result<…>` so it's easy to unit-test against a
//! wiremock server.

use std::time::Duration;

use crate::presenter::payload::PresenterPayload;

#[derive(Debug, thiserror::Error)]
pub enum PresenterError {
    #[error("presenter push timed out after {0:?}")]
    Timeout(Duration),
    #[error("presenter rejected push: HTTP {0}")]
    Rejected(u16),
    #[error("transport: {0}")]
    Transport(String),
}

#[derive(Clone)]
pub struct PresenterClient {
    client: reqwest::Client,
    endpoint: String,
    timeout: Duration,
}

impl PresenterClient {
    pub fn new(endpoint: String) -> Self {
        Self {
            client: reqwest::Client::builder()
                .build()
                .expect("reqwest client build"),
            endpoint,
            timeout: Duration::from_secs(2),
        }
    }

    pub async fn push(&self, payload: PresenterPayload) -> Result<(), PresenterError> {
        let resp = self
            .client
            .put(&self.endpoint)
            .json(&payload)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    PresenterError::Timeout(self.timeout)
                } else {
                    PresenterError::Transport(e.to_string())
                }
            })?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(PresenterError::Rejected(status.as_u16()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn payload() -> PresenterPayload {
        PresenterPayload {
            current_text: "line A".to_string(),
            next_text: "line B".to_string(),
            current_song: "Song X".to_string(),
            next_song: "Song Y".to_string(),
        }
    }

    #[tokio::test]
    async fn push_success_returns_ok_on_204() {
        let mock = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/stage"))
            .and(header("content-type", "application/json"))
            .and(body_json(serde_json::json!({
                "currentText": "line A",
                "nextText": "line B",
                "currentSong": "Song X",
                "nextSong": "Song Y"
            })))
            .respond_with(ResponseTemplate::new(204))
            .mount(&mock)
            .await;
        let client = PresenterClient::new(format!("{}/api/stage", mock.uri()));
        client.push(payload()).await.expect("204 is success");
    }

    #[tokio::test]
    async fn push_rejected_returns_status_error() {
        let mock = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(400))
            .mount(&mock)
            .await;
        let client = PresenterClient::new(format!("{}/api/stage", mock.uri()));
        let err = client.push(payload()).await.expect_err("must surface 400");
        assert!(matches!(err, PresenterError::Rejected(400)), "got {err:?}");
    }

    #[tokio::test]
    async fn push_timeout_surfaces_timeout_variant() {
        let mock = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(204).set_delay(Duration::from_secs(5)))
            .mount(&mock)
            .await;
        let mut client = PresenterClient::new(format!("{}/api/stage", mock.uri()));
        client.timeout = Duration::from_millis(200);
        let err = client
            .push(payload())
            .await
            .expect_err("slow responder must time out");
        assert!(matches!(err, PresenterError::Timeout(_)), "got {err:?}");
    }
}
