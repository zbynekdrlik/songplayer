//! Resolume/NDI health + lyrics quarantine handler tests — split out of
//! routes_tests.rs for the 1000-line cap (a child module of `tests`, so
//! `super::*` sees routes.rs items and the test_state helpers).

#![allow(unused_imports)]

use super::*;

#[tokio::test]
async fn resolume_health_endpoint_returns_array() {
    let state = test_state().await;
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/resolume/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v.is_array(), "response must be a JSON array");
}

/// Verifies the endpoint returns the registered hosts (not an empty Vec).
/// Kills the `get_resolume_health -> Json::from(vec![])` mutant.
#[tokio::test]
async fn resolume_health_endpoint_returns_registered_hosts() {
    let mut state = test_state().await;
    // Replace the empty Arc<ResolumeRegistry> with a populated one.
    let (shutdown_tx, _) = broadcast::channel::<()>(1);
    let mut registry = crate::resolume::ResolumeRegistry::new();
    registry.add_host(1, "10.0.0.99".to_string(), 8090, shutdown_tx.subscribe());
    state.resolume_registry = Arc::new(registry);

    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/resolume/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = v.as_array().expect("response must be a JSON array");
    assert_eq!(arr.len(), 1, "response should contain exactly one host");
    assert_eq!(
        arr[0]["host"].as_str(),
        Some("10.0.0.99"),
        "response must carry the registered host name"
    );

    let _ = shutdown_tx.send(());
}

#[tokio::test]
async fn ndi_health_endpoint_returns_array() {
    let state = test_state().await;
    let resp = app(state)
        .oneshot(
            Request::builder()
                .uri("/api/v1/ndi/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v.is_array(), "response must be a JSON array");
}

/// Kills the `get_ndi_health -> Json::from(vec![])` mutant.
/// Mirrors `resolume_health_endpoint_returns_registered_hosts` from PR #54.
#[tokio::test]
async fn ndi_health_endpoint_returns_seeded_pipeline() {
    use crate::playback::ndi_health::{PipelineHealthSnapshot, PlaybackStateLabel};
    use sp_core::genlock::lock_state::LockState;
    let state = test_state().await;
    state.ndi_health_registry.update(PipelineHealthSnapshot {
        playlist_id: 11,
        ndi_name: "SP-test".to_string(),
        state: PlaybackStateLabel::Playing,
        connections: 1,
        frames_submitted_total: 100,
        frames_submitted_last_5s: 30,
        observed_fps: 29.97,
        nominal_fps: 29.97,
        last_submit_ts: None,
        last_heartbeat_ts: None,
        consecutive_bad_polls: 0,
        degraded_reason: None,
        clock: crate::playback::clock_health::ClockHealth::default(),
        pacing: Default::default(),
        audio: Default::default(),
        // #149 Lane 1: flag-OFF (pacing disabled) reports UNLOCKED by contract.
        lock_state: LockState::Unlocked,
        lock_reason: "pacing disabled".to_string(),
        burn_on: false,
        recovery_step: None,
        sender_url: None,
    });
    let resp = app(state)
        .oneshot(
            Request::builder()
                .uri("/api/v1/ndi/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = v.as_array().expect("response must be a JSON array");
    assert_eq!(arr.len(), 1, "response should contain exactly one pipeline");
    assert_eq!(arr[0]["playlist_id"].as_i64(), Some(11));
    assert_eq!(arr[0]["ndi_name"].as_str(), Some("SP-test"));
    assert_eq!(arr[0]["state"], serde_json::json!("Playing"));
    // #149 Lane 1: lock_state + lock_reason are on the wire; a pacing-disabled
    // (flag-OFF) pipeline reports the three-state UNLOCKED vocabulary.
    assert_eq!(arr[0]["lock_state"].as_str(), Some("UNLOCKED"));
    assert_eq!(arr[0]["lock_reason"].as_str(), Some("pacing disabled"));
}

#[tokio::test]
async fn quarantine_endpoint_marks_row_and_deletes_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = tmp.path().to_path_buf();
    let state = test_state_with_cache_dir(cache_dir.clone()).await;

    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (1, 'p', 'u', 'n', 1)",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, has_lyrics, \
         lyrics_source, lyrics_pipeline_version) VALUES \
             (42, 1, 'ytQUAR', 1, 1, 'ensemble:gemini', 20)",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    let cache_file = cache_dir.join("ytQUAR_lyrics.json");
    tokio::fs::write(&cache_file, b"{\"version\":20,\"lines\":[]}")
        .await
        .unwrap();

    let resp = app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/lyrics/quarantine")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "video_id": 42,
                        "reason": "ASR missed bridge"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["video_id"], 42);
    assert_eq!(json["youtube_id"], "ytQUAR");
    assert_eq!(json["previous_source"], "ensemble:gemini");
    assert_eq!(json["deleted_cache_file"], true);

    assert!(!cache_file.exists(), "cache file must be gone");
    let source: String = sqlx::query_scalar("SELECT lyrics_source FROM videos WHERE id = 42")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(source, "asr_gap");
}

#[tokio::test]
async fn quarantine_endpoint_returns_404_for_missing_video() {
    let tmp = tempfile::tempdir().unwrap();
    let state = test_state_with_cache_dir(tmp.path().to_path_buf()).await;
    let resp = app(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/lyrics/quarantine")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"video_id": 999, "reason": ""}))
                        .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
