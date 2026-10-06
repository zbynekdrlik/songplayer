//! Unit tests of the OBS WebSocket client (`obs/mod.rs`), split out for the
//! 1000-line cap (#213). Wired via `#[cfg(test)] #[path = "mod_tests.rs"] mod tests;`.

use base64::Engine;
use sha2::{Digest, Sha256};

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
    // #154: idle-gate signals default to "not busy".
    assert!(!state.streaming);
    assert!(!state.recording);
}

#[test]
fn a_disconnect_forgets_everything_of_cg_obs() {
    let mut state = ObsState {
        connected: true,
        streaming: true,
        recording: true,
    };
    state.reset_disconnected();
    assert!(!state.connected);
    assert!(
        !state.streaming && !state.recording,
        "#154 idle-gate signals"
    );
}

/// #221 L6: the client subscribes to cg OBS's Scenes events (the facade
/// passes `SceneListChanged` on) and its Outputs events (the #154 stream /
/// record state); no Transitions any more (cg OBS's transition is no longer
/// read).
#[test]
fn the_identify_subscribes_scenes_and_outputs_only() {
    const SCENES: u64 = 1 << 2;
    const TRANSITIONS: u64 = 1 << 4;
    const OUTPUTS: u64 = 1 << 6;
    assert_eq!(EVENT_SUBSCRIPTIONS, SCENES | OUTPUTS);
    assert_eq!(EVENT_SUBSCRIPTIONS & TRANSITIONS, 0);
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
/// the connection loop never joins (one helper per title text, ladder rung
/// and rebuild, for the life of the connection). Every spawn first reaps the
/// finished helpers.
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
