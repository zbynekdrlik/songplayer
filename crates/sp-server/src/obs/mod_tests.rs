//! Unit tests of the OBS WebSocket client (`obs/mod.rs`), split out for the
//! 1000-line cap (#213). Wired via `#[cfg(test)] #[path = "mod_tests.rs"] mod tests;`.

use std::collections::HashMap;

use base64::Engine;
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

use super::*;

#[test]
fn test_compute_auth() {
    // Known test vectors: deterministic given password, challenge, salt.
    let password = "supersecretpassword";
    let challenge = "aDf8sUpKlMQIHOAd3dqr7KHLGr1Y1P4R";
    let salt = "lM1GncleQOaCu7U0knJcR5Tk3MFGz0VQ";

    let result = compute_auth(password, challenge, salt);

    // Verify it produces a valid base64 string.
    let engine = base64::engine::general_purpose::STANDARD;
    let decoded = engine.decode(&result);
    assert!(decoded.is_ok(), "result should be valid base64");
    assert_eq!(
        decoded.unwrap().len(),
        32,
        "SHA-256 output should be 32 bytes"
    );

    // Verify determinism.
    let result2 = compute_auth(password, challenge, salt);
    assert_eq!(result, result2);
}

#[test]
fn test_compute_auth_known_value() {
    // Manually compute expected value:
    // secret = base64(sha256("supersecretpassword" + "salt123"))
    // auth = base64(sha256(secret + "challenge456"))
    let password = "test";
    let salt = "salt123";
    let challenge = "challenge456";

    let engine = base64::engine::general_purpose::STANDARD;

    // Step 1: secret = base64(sha256(password + salt))
    let secret = engine.encode(Sha256::digest(format!("{password}{salt}").as_bytes()));
    // Step 2: auth = base64(sha256(secret + challenge))
    let expected = engine.encode(Sha256::digest(format!("{secret}{challenge}").as_bytes()));

    let result = compute_auth(password, challenge, salt);
    assert_eq!(result, expected);
}

#[test]
fn test_obs_state_default() {
    let state = ObsState::default();
    assert!(!state.connected);
    assert!(state.current_scene.is_none());
    assert!(state.active_playlist_ids.is_empty());
    // #154: idle-gate signals default to "not busy".
    assert!(!state.streaming);
    assert!(!state.recording);
}

#[test]
fn test_parse_hello_message() {
    let hello = serde_json::json!({
        "op": 0,
        "d": {
            "obsWebSocketVersion": "5.0.0",
            "rpcVersion": 1,
            "authentication": {
                "challenge": "aDf8sUpKlMQIHOAd3dqr7KHLGr1Y1P4R",
                "salt": "lM1GncleQOaCu7U0knJcR5Tk3MFGz0VQ"
            }
        }
    });

    assert_eq!(hello["op"].as_u64(), Some(0));
    let auth = hello["d"]["authentication"].as_object().unwrap();
    assert!(auth.contains_key("challenge"));
    assert!(auth.contains_key("salt"));
}

#[test]
fn test_parse_identified_message() {
    let identified = serde_json::json!({
        "op": 2,
        "d": {
            "negotiatedRpcVersion": 1
        }
    });

    assert_eq!(identified["op"].as_u64(), Some(2));
    assert_eq!(identified["d"]["negotiatedRpcVersion"].as_u64(), Some(1));
}

#[test]
fn test_parse_event_message() {
    let event = serde_json::json!({
        "op": 5,
        "d": {
            "eventType": "CurrentProgramSceneChanged",
            "eventData": {
                "sceneName": "Main Scene"
            }
        }
    });

    assert_eq!(event["op"].as_u64(), Some(5));
    assert_eq!(
        event["d"]["eventType"].as_str(),
        Some("CurrentProgramSceneChanged")
    );
    assert_eq!(
        event["d"]["eventData"]["sceneName"].as_str(),
        Some("Main Scene")
    );
}

#[test]
fn test_parse_request_response() {
    let response = serde_json::json!({
        "op": 7,
        "d": {
            "requestType": "GetCurrentProgramScene",
            "requestId": "abc-123",
            "requestStatus": {
                "result": true,
                "code": 100
            },
            "responseData": {
                "currentProgramSceneName": "Live Scene"
            }
        }
    });

    assert_eq!(response["op"].as_u64(), Some(7));
    assert_eq!(
        response["d"]["requestType"].as_str(),
        Some("GetCurrentProgramScene")
    );
    assert_eq!(
        response["d"]["responseData"]["currentProgramSceneName"].as_str(),
        Some("Live Scene")
    );
}

#[test]
fn test_state_connected_transition() {
    let mut state = ObsState::default();
    assert!(!state.connected);

    state.connected = true;
    state.current_scene = Some("Scene 1".to_string());
    state.active_playlist_ids.insert(1);
    state.active_playlist_ids.insert(2);

    assert!(state.connected);
    assert_eq!(state.current_scene.as_deref(), Some("Scene 1"));
    assert!(state.active_playlist_ids.contains(&1));
    assert!(state.active_playlist_ids.contains(&2));

    // Disconnect transition.
    state.connected = false;
    state.current_scene = None;
    state.active_playlist_ids.clear();

    assert!(!state.connected);
    assert!(state.current_scene.is_none());
    assert!(state.active_playlist_ids.is_empty());
}

#[test]
fn test_parse_hello_without_auth() {
    let hello = serde_json::json!({
        "op": 0,
        "d": {
            "obsWebSocketVersion": "5.0.0",
            "rpcVersion": 1
        }
    });

    assert_eq!(hello["op"].as_u64(), Some(0));
    assert!(hello["d"]["authentication"].as_object().is_none());
}

// ---- apply_rebuild_result: preserve-on-None regression guard ----
//
// These tests encode the fix for the 2026-04-19 event outage: the
// NDI source map was built at startup, then a later rebuild returned
// empty (OBS `GetInputList` responded with nothing), and the old
// code overwrote the map. Every scene change after that matched
// against the empty map and the engine received no commands.
// Switching OBS scenes became a silent no-op for hours.

#[tokio::test]
async fn apply_rebuild_result_writes_when_rebuild_succeeds() {
    let lock = RwLock::new(HashMap::new());
    let mut new_map = HashMap::new();
    new_map.insert("sp-fast_video".into(), 7i64);

    apply_rebuild_result(&lock, Some(new_map.clone())).await;

    let guard = lock.read().await;
    assert_eq!(*guard, new_map);
}

#[tokio::test]
async fn apply_rebuild_result_preserves_existing_map_when_rebuild_returns_none() {
    // Seed the map with production-shaped data — what startup built.
    let mut seed = HashMap::new();
    seed.insert("sp-warmup_video".into(), 2i64);
    seed.insert("sp-fast_video".into(), 7i64);
    seed.insert("sp-worship_video".into(), 6i64);
    let lock = RwLock::new(seed.clone());

    // Simulate a transient OBS failure: rebuild returned None.
    apply_rebuild_result(&lock, None).await;

    let guard = lock.read().await;
    assert_eq!(
        *guard, seed,
        "None result MUST preserve the previous map — overwriting \
         with empty on query failure is what broke the 2026-04-19 \
         event"
    );
}

#[tokio::test]
async fn apply_rebuild_result_replaces_map_with_empty_when_rebuild_legitimately_empty() {
    // A Some(empty) is a real signal: DB truly has no active playlists
    // or OBS truly has no NDI inputs. That SHOULD wipe the map.
    // (Contrast with None, which means the query failed.)
    let mut seed = HashMap::new();
    seed.insert("sp-fast_video".into(), 7i64);
    let lock = RwLock::new(seed);

    apply_rebuild_result(&lock, Some(HashMap::new())).await;

    let guard = lock.read().await;
    assert!(
        guard.is_empty(),
        "a legitimate empty rebuild (user removed all inputs) MUST \
         wipe the map so scene detection reflects current reality"
    );
}
