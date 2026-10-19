//! Non-blocking HTTP PUT to the Presenter stage-display API.
//!
//! Callers hand a payload to `PresenterClient::send` (#281): fire-and-forget,
//! so playback never blocks on network I/O; one task per client sends the
//! NEWEST payload, one push at a time, and a failed one again until it goes,
//! so the stage display ends on the last payload sent. `push` itself stays a
//! plain `async fn Result<…>` so it's easy to unit-test against a wiremock
//! server.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::watch;

use crate::presenter::payload::PresenterPayload;

/// #281: the newest payload, with what sent it (the failure's WARN).
type Queued = (PresenterPayload, &'static str);

/// #281: a failed push is sent again after `RETRY_FIRST`, doubling up to
/// `RETRY_MAX` while it keeps failing (`retry_wait`).
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(60);

/// #281: the wait before the `failures`-th failed attempt in a row is sent
/// again: `first` doubled per earlier failure, at most `RETRY_MAX`.
fn retry_wait(first: Duration, failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    first.saturating_mul(1 << doublings).min(RETRY_MAX)
}

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
    /// #281: the first wait before a failed push is sent again (`retry_wait`).
    retry_first: Duration,
    /// #281: the newest payload, read by the one task that sends this
    /// client's pushes, started by the first `send`.
    queue: Arc<OnceLock<watch::Sender<Option<Queued>>>>,
}

impl PresenterClient {
    pub fn new(endpoint: String) -> Self {
        Self {
            client: reqwest::Client::builder()
                .build()
                .expect("reqwest client build"),
            endpoint,
            timeout: Duration::from_secs(2),
            retry_first: RETRY_FIRST,
            queue: Arc::new(OnceLock::new()),
        }
    }

    /// #281: `payload` for the stage display. One task per client sends the
    /// NEWEST payload, one push at a time: pushes spawned apart could arrive
    /// the other way round (a clear landing after the title sent after it),
    /// and the display would end on the older one; a payload replaced while
    /// a push is out is never sent (the display shows only the newest). A
    /// failed push is sent again (`retry_wait`) until it goes or a newer one
    /// replaces it: a song with no lyrics pushes its title once. A failure
    /// is a WARN naming `why`. Called inside the tokio runtime (the first
    /// call starts the task).
    pub fn send(&self, payload: PresenterPayload, why: &'static str) {
        let latest = self.queue.get_or_init(|| {
            let (tx, rx) = watch::channel(None);
            tokio::spawn(run_queue(self.detached(), rx));
            tx
        });
        latest.send_replace(Some((payload, why)));
    }

    /// The same endpoint with a queue of its own, for the sending task: it
    /// must not hold the sender of the queue it reads, or that queue would
    /// never close.
    fn detached(&self) -> Self {
        Self {
            client: self.client.clone(),
            endpoint: self.endpoint.clone(),
            timeout: self.timeout,
            retry_first: self.retry_first,
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

/// #281: send the newest payload whenever one comes, a failed one again
/// after `retry_wait` unless a newer one replaces it, until the sender is
/// gone (every clone of its client dropped).
async fn run_queue(client: PresenterClient, mut latest: watch::Receiver<Option<Queued>>) {
    while latest.changed().await.is_ok() {
        let mut failures = 0;
        loop {
            let Some((payload, why)) = latest.borrow_and_update().clone() else {
                break;
            };
            let Err(e) = client.push(payload).await else {
                break;
            };
            failures += 1;
            let wait = retry_wait(client.retry_first, failures);
            tracing::warn!(
                ?e,
                why,
                ?wait,
                "presenter push failed (non-fatal): sent again"
            );
            tokio::select! {
                changed = latest.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                () = tokio::time::sleep(wait) => {}
            }
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

    /// The bodies the mock received, once there are at least `n` (bounded:
    /// 20 s).
    async fn bodies(mock: &MockServer, n: usize) -> Vec<serde_json::Value> {
        for _ in 0..400 {
            let got = mock.received_requests().await.unwrap_or_default();
            if got.len() >= n {
                return got
                    .iter()
                    .map(|r| serde_json::from_slice(&r.body).expect("a JSON body"))
                    .collect();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the mock did not receive {n} push(es) within 20 s");
    }

    fn texts(bodies: &[serde_json::Value]) -> Vec<serde_json::Value> {
        bodies.iter().map(|b| b["currentText"].clone()).collect()
    }

    /// #281: one push at a time, the newest payload: while the first push
    /// waits for its answer, B and then C are sent; only C goes after it (the
    /// display needs only the newest), and nothing more (a "no more" window
    /// that only errs in the safe direction).
    #[tokio::test]
    async fn sent_pushes_go_one_at_a_time_and_the_newest_wins() {
        let mock = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(204).set_delay(Duration::from_millis(800)))
            .mount(&mock)
            .await;
        let mut client = PresenterClient::new(format!("{}/api/stage", mock.uri()));
        client.timeout = Duration::from_secs(10);
        let with_text = |text: &str| {
            let mut p = payload();
            p.current_text = text.to_string();
            p
        };
        client.send(with_text("A"), "a");
        bodies(&mock, 1).await;
        client.send(with_text("B"), "b");
        client.send(with_text("C"), "c");

        assert_eq!(texts(&bodies(&mock, 2).await), ["A", "C"]);
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(texts(&bodies(&mock, 2).await), ["A", "C"], "B never went");
    }

    /// #281: a failed push is sent again until it goes: the stage display
    /// must not lose a song's title to one failed request.
    #[tokio::test]
    async fn a_failed_push_is_sent_again() {
        let mock = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(2)
            .mount(&mock)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&mock)
            .await;
        let mut client = PresenterClient::new(format!("{}/api/stage", mock.uri()));
        client.retry_first = Duration::from_millis(20);
        client.send(payload(), "the title");

        let got = bodies(&mock, 3).await;
        assert_eq!(texts(&got), ["line A", "line A", "line A"]);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            bodies(&mock, 3).await.len(),
            3,
            "not sent again once it went"
        );
    }

    #[test]
    fn a_failed_push_waits_longer_each_time_up_to_a_minute() {
        let s = Duration::from_secs;
        let waits: Vec<_> = (1..=8).map(|n| retry_wait(s(1), n)).collect();
        assert_eq!(waits, [s(1), s(2), s(4), s(8), s(16), s(32), s(60), s(60)]);
        assert_eq!(retry_wait(s(1), 0), s(1), "no failure counts as the first");
        assert_eq!(retry_wait(s(3), 2), s(6));
        assert_eq!(retry_wait(s(1), u32::MAX), s(60));
        assert_eq!((RETRY_FIRST, RETRY_MAX), (s(1), s(60)));
    }
}
