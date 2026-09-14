//! #154: OBS stream/record state tracking for the lyrics idle gate.
//!
//! The lyrics worker defers heavy GPU/CPU work while OBS is live
//! (`ObsState.streaming/recording`). Those flags are updated from
//! `StreamStateChanged` / `RecordStateChanged` events in the reader, but an
//! output already active when SongPlayer connects fires no such event — so this
//! module seeds the state once on (re)connect. Extracted from `mod.rs` to keep
//! that file under the 1000-line cap.

use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, warn};

use crate::obs::ObsState;
use crate::obs::SharedWrite;
use crate::obs::dispatcher::{DEFAULT_RESPONSE_TIMEOUT, Dispatcher, DispatcherError};

/// Seed `ObsState.streaming/recording` from OBS on (re)connect via
/// `GetStreamStatus` / `GetRecordStatus`. Best-effort: a failure leaves the
/// state at its default (not busy); the next state-change event corrects it.
pub(crate) async fn seed_output_state(
    write: &SharedWrite,
    dispatcher: &Dispatcher,
    state: &Arc<RwLock<ObsState>>,
) {
    for (req_type, recording) in [("GetStreamStatus", false), ("GetRecordStatus", true)] {
        let req_id = uuid::Uuid::new_v4().to_string();
        let req = serde_json::json!({
            "op": 6,
            "d": { "requestType": req_type, "requestId": req_id.clone() }
        });
        match dispatcher
            .send_and_await(
                write,
                req_id,
                Message::Text(req.to_string().into()),
                DEFAULT_RESPONSE_TIMEOUT,
            )
            .await
        {
            Ok(response) => {
                if let Some(active) = response["d"]["responseData"]["outputActive"].as_bool() {
                    let mut s = state.write().await;
                    if recording {
                        s.recording = active;
                    } else {
                        s.streaming = active;
                    }
                    debug!(
                        req_type,
                        active, "seeded OBS output state (idle-gate signal)"
                    );
                }
            }
            Err(DispatcherError::Closed) => {
                debug!("reader closed during initial {req_type}; main loop will reconnect");
            }
            Err(DispatcherError::Timeout) => {
                warn!(
                    "initial {req_type} timed out — idle gate relies on events until next change"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::obs::ObsState;

    #[test]
    fn parse_stream_state_changed_output_active() {
        // #154: the reader extracts `outputActive` from StreamStateChanged /
        // RecordStateChanged to drive the idle gate.
        let ev = serde_json::json!({
            "op": 5,
            "d": {
                "eventType": "StreamStateChanged",
                "eventData": { "outputActive": true, "outputState": "OBS_WEBSOCKET_OUTPUT_STARTED" }
            }
        });
        assert_eq!(ev["d"]["eventType"].as_str(), Some("StreamStateChanged"));
        assert_eq!(ev["d"]["eventData"]["outputActive"].as_bool(), Some(true));

        let rec_stopped = serde_json::json!({
            "op": 5,
            "d": {
                "eventType": "RecordStateChanged",
                "eventData": { "outputActive": false, "outputState": "OBS_WEBSOCKET_OUTPUT_STOPPED" }
            }
        });
        assert_eq!(
            rec_stopped["d"]["eventData"]["outputActive"].as_bool(),
            Some(false)
        );
    }

    #[test]
    fn parse_output_status_response_seed() {
        // #154 initial seed: GetStreamStatus / GetRecordStatus responses carry
        // `outputActive` under responseData.
        let resp = serde_json::json!({
            "op": 7,
            "d": { "responseData": { "outputActive": true, "outputDuration": 123 } }
        });
        assert_eq!(
            resp["d"]["responseData"]["outputActive"].as_bool(),
            Some(true)
        );
    }

    #[test]
    fn obs_state_output_flags_settable() {
        let mut state = ObsState::default();
        assert!(!state.streaming && !state.recording);
        state.streaming = true;
        assert!(state.streaming && !state.recording);
        state.recording = true;
        state.streaming = false;
        assert!(state.recording && !state.streaming);
    }
}
