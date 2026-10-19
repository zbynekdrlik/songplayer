//! Non-blocking HTTP PUT to the Presenter stage-display API.
//!
//! Callers queue a push with `PresenterClient::send` (#281): fire-and-forget,
//! so playback never blocks on network I/O, and in order, so the stage
//! display ends on the last payload queued. `push` itself stays a plain
//! `async fn Result<…>` so it's easy to unit-test against a wiremock server.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::presenter::payload::PresenterPayload;

/// #281: a push waiting for its turn, with what queued it (the failure's WARN).
type Queued = (PresenterPayload, &'static str);

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
    /// #281: the queue of the one task that sends this client's pushes,
    /// started by the first `send`.
    queue: Arc<OnceLock<mpsc::UnboundedSender<Queued>>>,
}

impl PresenterClient {
    pub fn new(endpoint: String) -> Self {
        Self {
            client: reqwest::Client::builder()
                .build()
                .expect("reqwest client build"),
            endpoint,
            timeout: Duration::from_secs(2),
            queue: Arc::new(OnceLock::new()),
        }
    }

    /// #281: queue `payload` for the stage display. One task per client
    /// sends the queued pushes one after another, in queue order: two pushes
    /// spawned apart could arrive the other way round (a clear landing after
    /// the title queued after it), and the display would end on the older
    /// one. A failure is a WARN naming `why`. Called inside the tokio
    /// runtime (the first call starts the task).
    pub fn send(&self, payload: PresenterPayload, why: &'static str) {
        let queue = self.queue.get_or_init(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            tokio::spawn(run_queue(self.detached(), rx));
            tx
        });
        if queue.send((payload, why)).is_err() {
            tracing::warn!(why, "presenter push queue closed (non-fatal)");
        }
    }

    /// The same endpoint with a queue of its own, for the sending task: it
    /// must not hold the sender of the queue it reads, or that queue would
    /// never close.
    fn detached(&self) -> Self {
        Self {
            client: self.client.clone(),
            endpoint: self.endpoint.clone(),
            timeout: self.timeout,
            queue: Arc::new(OnceLock::new()),
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

/// #281: send the queued pushes one after another until the queue closes
/// (every clone of its client dropped).
async fn run_queue(client: PresenterClient, mut queue: mpsc::UnboundedReceiver<Queued>) {
    while let Some((payload, why)) = queue.recv().await {
        if let Err(e) = client.push(payload).await {
            tracing::warn!(?e, why, "presenter push failed (non-fatal)");
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
            current_translation: "riadok A".to_string(),
            next_translation: String::new(),
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
                "nextSong": "Song Y",
                "currentTranslation": "riadok A",
                "nextTranslation": ""
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

    /// #281: queued pushes go out one after another, in queue order: the
    /// second is not sent while the first waits for its answer (a "not yet"
    /// window that only errs in the safe direction), then both arrive, the
    /// first one first.
    #[tokio::test]
    async fn sent_pushes_go_out_one_after_another_in_order() {
        let mock = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(204).set_delay(Duration::from_millis(600)))
            .mount(&mock)
            .await;
        let client = PresenterClient::new(format!("{}/api/stage", mock.uri()));
        let mut second = payload();
        second.current_text = "line C".to_string();
        client.send(payload(), "first");
        client.send(second, "second");

        tokio::time::sleep(Duration::from_millis(300)).await;
        let got = mock.received_requests().await.unwrap_or_default();
        assert_eq!(got.len(), 1, "the second waits for the first's answer");
        let mut bodies = Vec::new();
        for _ in 0..200 {
            let got = mock.received_requests().await.unwrap_or_default();
            if got.len() >= 2 {
                bodies = got
                    .iter()
                    .map(|r| serde_json::from_slice::<serde_json::Value>(&r.body).unwrap())
                    .collect();
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let texts: Vec<_> = bodies.iter().map(|b| b["currentText"].clone()).collect();
        assert_eq!(texts, ["line A", "line C"], "both, in queue order");
    }
}
