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
    assert!(state.lookup_failed.is_none(), "#218: no failed lookup");
    // #154: idle-gate signals default to "not busy".
    assert!(!state.streaming);
    assert!(!state.recording);
    assert!(state.transition.is_none(), "#219: transition unknown");
}

#[test]
fn a_disconnect_forgets_everything_of_cg_obs() {
    // #219: every published field back to the default (disconnected)
    // snapshot — a consumer reads a disconnect from exactly this.
    let mut state = ObsState {
        connected: true,
        current_scene: Some("sp-fast".to_string()),
        active_playlist_ids: std::collections::HashSet::from([7]),
        lookup_failed: Some("sp-fast".to_string()),
        streaming: true,
        recording: true,
        transition: Some(ObsTransition {
            name: "Fade".to_string(),
            kind: "fade_transition".to_string(),
            duration_ms: Some(300),
        }),
    };
    state.reset_disconnected();
    assert_eq!(ObsSnapshot::of(&state), ObsSnapshot::default());
    assert!(
        !state.streaming && !state.recording,
        "#154 idle-gate signals"
    );
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

/// The dashboard (sp-ui Nastavenia) stores the OBS password under
/// `SETTING_OBS_WEBSOCKET_PASSWORD` ("obs_websocket_password"), but startup
/// read "obs_password", so a password set in the UI never reached the client
/// (found in the #213 review).
#[tokio::test]
async fn the_obs_config_reads_the_keys_the_dashboard_writes() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    assert_eq!(
        load_obs_config(&pool).await.unwrap(),
        None,
        "no URL, no client"
    );
    let set = |k: &'static str, v: &'static str| {
        let pool = pool.clone();
        async move { crate::db::models::set_setting(&pool, k, v).await.unwrap() }
    };
    set(
        sp_core::config::SETTING_OBS_WEBSOCKET_URL,
        "ws://10.0.0.5:4455",
    )
    .await;
    let cfg = load_obs_config(&pool)
        .await
        .unwrap()
        .expect("a URL configures OBS");
    assert_eq!(cfg.url, "ws://10.0.0.5:4455");
    assert_eq!(cfg.password, None, "an empty password means no auth");
    set(sp_core::config::SETTING_OBS_WEBSOCKET_PASSWORD, "s3cret").await;
    let cfg = load_obs_config(&pool).await.unwrap().unwrap();
    assert_eq!(cfg.password.as_deref(), Some("s3cret"));
}

/// Review round 4: a `JoinSet` keeps a finished task until it is joined, and
/// the connection loop never joins — its ~2 s scene poll alone spawns ~43 000
/// helper tasks a day. Every spawn first reaps the finished helpers.
#[tokio::test]
async fn spawning_a_helper_reaps_the_finished_ones() {
    let mut tasks: JoinSet<()> = JoinSet::new();
    for _ in 0..3 {
        spawn_helper(&mut tasks, async {});
    }
    assert_eq!(tasks.len(), 3);
    // Current-thread runtime: the three run to completion while this yields.
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    spawn_helper(&mut tasks, std::future::pending::<()>());
    assert_eq!(tasks.len(), 1, "the three finished helpers were reaped");
    tasks.abort_all();
}
