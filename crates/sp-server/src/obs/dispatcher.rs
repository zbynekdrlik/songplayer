//! Response dispatcher for the OBS WebSocket client.
//!
//! Owns a `request_id → oneshot::Sender<Value>` pending map. A single
//! reader task reads every inbound op=7 message and calls
//! `Dispatcher::complete(req_id, payload)`, which forwards the payload
//! to the waiter registered for that request_id. Callers register
//! before sending the request, send via the SplitSink half, and await
//! the receiver with `await_response`.
//!
//! Replaces the per-call `wait_for_response` helpers in
//! `ndi_discovery.rs` and `scene.rs` that consumed and dropped any
//! op=5 event arriving while a request was in flight (issue #43).

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{Mutex, oneshot};
use tokio::time::timeout;
use tracing::warn;

pub const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum DispatcherError {
    Timeout,
    Closed,
}

#[derive(Clone, Default)]
pub struct Dispatcher {
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
}

impl Dispatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a waiter for a future op=7 response with the given
    /// `request_id`. Returns the receiver half; the caller awaits it
    /// (typically via `await_response`).
    ///
    /// If a waiter is already registered for the same `request_id`,
    /// it is replaced. The previous oneshot::Sender is dropped and
    /// its receiver observes an `Err(_)`. Callers MUST generate
    /// unique UUID request IDs (all current call sites already do
    /// via `uuid::Uuid::new_v4`).
    pub async fn register(&self, req_id: String) -> oneshot::Receiver<Value> {
        let (tx, rx) = oneshot::channel();
        let mut guard = self.pending.lock().await;
        guard.insert(req_id, tx);
        rx
    }

    /// Register + await + per-call timeout. Returns the response JSON
    /// on success, `Timeout` if `timeout_dur` elapses before a
    /// matching op=7 arrives, or `Closed` if the dispatcher was
    /// drained while the call was pending.
    ///
    /// On timeout the registration is removed so a late-arriving
    /// response does not leak into the pending map.
    pub async fn await_response(
        &self,
        req_id: String,
        timeout_dur: Duration,
    ) -> Result<Value, DispatcherError> {
        let rx = self.register(req_id.clone()).await;
        match timeout(timeout_dur, rx).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => Err(DispatcherError::Closed),
            Err(_) => {
                // Remove our stale entry so the reader task does not
                // try to deliver into a dropped channel later.
                let mut guard = self.pending.lock().await;
                guard.remove(&req_id);
                Err(DispatcherError::Timeout)
            }
        }
    }

    /// Forward an inbound op=7 payload to the registered waiter, if
    /// any. An unmatched response is logged at WARN — it means the
    /// sender gave up (timed out, was dropped) before the response
    /// arrived.
    pub async fn complete(&self, req_id: &str, value: Value) {
        let mut guard = self.pending.lock().await;
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

    /// Drop every pending waiter so its receiver observes `Err(_)`.
    /// Called by the reader task on connection close so no caller
    /// hangs on a never-arriving response.
    pub async fn drain_and_close(&self) {
        let mut guard = self.pending.lock().await;
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
        let rx = d.register("req-1".to_string()).await;

        let payload = json!({"op": 7, "d": {"requestId": "req-1", "ok": true}});
        d.complete("req-1", payload.clone()).await;

        let got = rx.await.expect("oneshot must deliver");
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn await_response_returns_payload_on_complete() {
        let d = Dispatcher::default();
        let d2 = d.clone();

        let waiter = tokio::spawn(async move {
            d2.await_response("req-2".to_string(), Duration::from_secs(1))
                .await
        });

        // Give the spawn time to register.
        tokio::time::sleep(Duration::from_millis(10)).await;

        let payload = json!({"op": 7, "d": {"requestId": "req-2"}});
        d.complete("req-2", payload.clone()).await;

        let result = waiter.await.expect("task joins").expect("ok");
        assert_eq!(result, payload);
    }

    #[tokio::test]
    async fn await_response_returns_timeout_when_no_response() {
        let d = Dispatcher::default();
        let result = d
            .await_response("req-3".to_string(), Duration::from_millis(50))
            .await;
        assert!(matches!(result, Err(DispatcherError::Timeout)));
    }

    #[tokio::test]
    async fn drain_and_close_fails_all_pending() {
        let d = Dispatcher::default();
        let rx_a = d.register("a".to_string()).await;
        let rx_b = d.register("b".to_string()).await;

        d.drain_and_close().await;

        assert!(rx_a.await.is_err(), "drain must close oneshot");
        assert!(rx_b.await.is_err(), "drain must close oneshot");
    }

    #[tokio::test]
    async fn complete_with_no_waiter_does_not_panic() {
        // Unmatched op=7 (sender gave up / timed out) MUST be a soft
        // case — log + drop, not crash.
        let d = Dispatcher::default();
        d.complete("nobody-cares", json!({})).await;
        // If we reach here without panicking the test passes.
    }

    #[tokio::test]
    async fn double_register_replaces_previous_waiter() {
        // Same request_id registered twice — the first waiter sees an
        // error on the dropped sender, the second receives the value.
        // This shape can only happen if a caller reuses an ID by
        // mistake; we want it to fail loudly on the first waiter,
        // not silently merge the channels.
        let d = Dispatcher::default();
        let rx_first = d.register("dup".to_string()).await;
        let rx_second = d.register("dup".to_string()).await;

        d.complete("dup", json!("v")).await;

        assert!(
            rx_first.await.is_err(),
            "first waiter must fail (sender dropped)"
        );
        assert_eq!(rx_second.await.expect("second wins"), json!("v"));
    }
}
