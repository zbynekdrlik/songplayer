//! Response dispatcher for the OBS WebSocket client.
//!
//! Owns a `request_id → oneshot::Sender<Value>` pending map. A single
//! reader task reads every inbound op=7 message and calls
//! `Dispatcher::complete(req_id, payload)`, which forwards the payload
//! to the waiter registered for that request_id. Callers use
//! `send_and_await` to register, send, and await in one call.
//!
//! Replaces the per-call `wait_for_response` helpers in
//! `ndi_discovery.rs` and `scene.rs` that consumed and dropped any
//! op=5 event arriving while a request was in flight (issue #43).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::oneshot;
use tokio::time::timeout;
use tracing::warn;

pub const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum DispatcherError {
    Timeout,
    Closed,
}

impl std::fmt::Display for DispatcherError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => write!(f, "OBS dispatcher: response timed out"),
            Self::Closed => write!(f, "OBS dispatcher: closed before reply"),
        }
    }
}

impl std::error::Error for DispatcherError {}

#[derive(Clone, Default)]
pub struct Dispatcher {
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
}

impl Dispatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a waiter for a future op=7 response with the given
    /// `request_id`. Returns the receiver half.
    ///
    /// If a waiter is already registered for the same `request_id`,
    /// it is replaced and a WARN is emitted. The previous
    /// oneshot::Sender is dropped and its receiver observes an
    /// `Err(_)`. Callers MUST generate unique UUID request IDs (all
    /// current call sites already do via `uuid::Uuid::new_v4`).
    pub fn register(&self, req_id: String) -> oneshot::Receiver<Value> {
        let (tx, rx) = oneshot::channel();
        let mut guard = self
            .pending
            .lock()
            .expect("dispatcher pending lock poisoned");
        if guard.contains_key(&req_id) {
            warn!(
                request_id = %req_id,
                "OBS dispatcher: duplicate registration — first waiter will fail"
            );
        }
        guard.insert(req_id, tx);
        rx
    }

    /// Register, send via `write`, then await the matching op=7 response
    /// with `timeout_dur`. Releases the write lock as soon as the send
    /// completes so concurrent helpers don't serialise on the response
    /// wait. Cleans up the pending entry on timeout / send-error.
    ///
    /// `req_id` MUST be unique (callers generate `uuid::Uuid::new_v4`).
    /// `msg` is the JSON request body already serialised as a `Message::Text`.
    pub async fn send_and_await(
        &self,
        write: &crate::obs::SharedWrite,
        req_id: String,
        msg: tokio_tungstenite::tungstenite::Message,
        timeout_dur: Duration,
    ) -> Result<Value, DispatcherError> {
        use futures::SinkExt;

        let rx = self.register(req_id.clone());
        {
            let mut w = write.lock().await;
            if w.send(msg).await.is_err() {
                drop(w);
                self.cancel(&req_id);
                return Err(DispatcherError::Closed);
            }
        }
        match timeout(timeout_dur, rx).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => Err(DispatcherError::Closed),
            Err(_) => {
                self.cancel(&req_id);
                Err(DispatcherError::Timeout)
            }
        }
    }

    /// Forward an inbound op=7 payload to the registered waiter, if
    /// any. An unmatched response is logged at WARN — it means the
    /// sender gave up (timed out, was dropped) before the response
    /// arrived.
    pub fn complete(&self, req_id: &str, value: Value) {
        let mut guard = self
            .pending
            .lock()
            .expect("dispatcher pending lock poisoned");
        match guard.remove(req_id) {
            Some(tx) => {
                // If the receiver was already dropped (e.g. caller
                // raced past its timeout), send returns Err — that's
                // a soft case, not a panic.
                let _ = tx.send(value);
            }
            None => {
                warn!(
                    request_id = req_id,
                    "OBS dispatcher: received op=7 with no registered \
                     waiter (sender likely timed out earlier)"
                );
            }
        }
    }

    /// Cancel a pending registration without delivering anything. Used
    /// by callers that registered a waiter but then failed to send the
    /// matching request (e.g. write-side error). Removes the entry from
    /// the pending map and drops the `oneshot::Sender` so any later
    /// arriving response is treated as unmatched + logged at WARN
    /// (cannot happen in practice for cancelled-before-send, but the
    /// cleanup keeps the map size bounded).
    pub fn cancel(&self, req_id: &str) {
        let mut guard = self
            .pending
            .lock()
            .expect("dispatcher pending lock poisoned");
        let _ = guard.remove(req_id);
    }

    /// Drop every pending waiter so its receiver observes `Err(_)`.
    /// Called by the reader task on connection close so no caller
    /// hangs on a never-arriving response.
    pub fn drain_and_close(&self) {
        let mut guard = self
            .pending
            .lock()
            .expect("dispatcher pending lock poisoned");
        guard.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    #[tokio::test]
    async fn register_and_complete_delivers_payload() {
        let d = Dispatcher::default();
        let rx = d.register("req-1".to_string());

        let payload = json!({"op": 7, "d": {"requestId": "req-1", "ok": true}});
        d.complete("req-1", payload.clone());

        let got = rx.await.expect("oneshot must deliver");
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn drain_and_close_fails_all_pending() {
        let d = Dispatcher::default();
        let rx_a = d.register("a".to_string());
        let rx_b = d.register("b".to_string());

        d.drain_and_close();

        assert!(rx_a.await.is_err(), "drain must close oneshot");
        assert!(rx_b.await.is_err(), "drain must close oneshot");
    }

    #[tokio::test]
    async fn complete_with_no_waiter_does_not_panic() {
        // Unmatched op=7 (sender gave up / timed out) MUST be a soft
        // case — log + drop, not crash.
        let d = Dispatcher::default();
        d.complete("nobody-cares", json!({}));
        // If we reach here without panicking the test passes.
    }

    #[tokio::test]
    async fn cancel_removes_pending_entry() {
        let d = Dispatcher::default();
        let rx = d.register("cancel-me".to_string());
        d.cancel("cancel-me");
        // Sender dropped → rx must observe Err.
        assert!(rx.await.is_err(), "cancel must drop the sender");
        // Subsequent complete is a soft no-op (no waiter).
        d.complete("cancel-me", serde_json::json!("late"));
    }

    #[tokio::test]
    async fn double_register_replaces_previous_waiter() {
        // Same request_id registered twice — the first waiter sees an
        // error on the dropped sender, the second receives the value.
        // This shape can only happen if a caller reuses an ID by
        // mistake; we want it to fail loudly on the first waiter,
        // not silently merge the channels.
        let d = Dispatcher::default();
        let rx_first = d.register("dup".to_string());
        let rx_second = d.register("dup".to_string());

        d.complete("dup", json!("v"));

        assert!(
            rx_first.await.is_err(),
            "first waiter must fail (sender dropped)"
        );
        assert_eq!(rx_second.await.expect("second wins"), json!("v"));
    }
}
