//! `POST /api/v1/ndi/burn` toggle + `burn_on` health exposure tests (#151).
//! Included via `#[path] #[cfg(test)] mod tests_burn;` from routes.rs; shares
//! `test_state`/`app` with `routes_tests.rs` via `super::tests`. Mirrors the
//! #147 pacing-exposure test.

#![allow(unused_imports)]

use super::tests::{app, test_state};
use super::*;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

fn burn_body(output: &str, on: bool) -> Body {
    Body::from(serde_json::json!({ "output": output, "on": on }).to_string())
}

async fn post_burn(state: crate::AppState, output: &str, on: bool) -> StatusCode {
    app(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/ndi/burn")
                .header("content-type", "application/json")
                .body(burn_body(output, on))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn burn_toggle_on_paced_output_returns_204_and_flips_flag() {
    let state = test_state().await;
    state.ndi_burn_registry.register("SP-fast", true); // paced

    assert_eq!(
        post_burn(state.clone(), "SP-fast", true).await,
        StatusCode::NO_CONTENT
    );
    assert!(
        state.ndi_burn_registry.is_on("SP-fast"),
        "204 must flip the shared flag on"
    );

    assert_eq!(
        post_burn(state.clone(), "SP-fast", false).await,
        StatusCode::NO_CONTENT
    );
    assert!(
        !state.ndi_burn_registry.is_on("SP-fast"),
        "toggling off clears within one call (one frame)"
    );
}

#[tokio::test]
async fn burn_toggle_unknown_output_returns_404() {
    let state = test_state().await;
    assert_eq!(
        post_burn(state, "SP-nope", true).await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn burn_toggle_on_non_paced_output_returns_409() {
    let state = test_state().await;
    state.ndi_burn_registry.register("SP-legacy", false); // pacing disabled

    assert_eq!(
        post_burn(state.clone(), "SP-legacy", true).await,
        StatusCode::CONFLICT
    );
    assert!(
        !state.ndi_burn_registry.is_on("SP-legacy"),
        "a 409 (pacing disabled) must NOT flip the flag"
    );
}

fn burn_health_snapshot(burn_on: bool) -> crate::playback::ndi_health::PipelineHealthSnapshot {
    use crate::playback::ndi_health::{AudioStats, PacingStats, PlaybackStateLabel};
    crate::playback::ndi_health::PipelineHealthSnapshot {
        playlist_id: 7,
        ndi_name: "SP-fast".to_string(),
        state: PlaybackStateLabel::Playing,
        connections: 1,
        frames_submitted_total: 5,
        frames_submitted_last_5s: 1,
        observed_fps: 30.0,
        nominal_fps: 30.0,
        last_submit_ts: None,
        last_heartbeat_ts: None,
        consecutive_bad_polls: 0,
        degraded_reason: None,
        clock: Default::default(),
        pacing: PacingStats {
            enabled: true,
            ..Default::default()
        },
        audio: AudioStats::default(),
        lock_state: sp_core::genlock::lock_state::LockState::Locked,
        lock_reason: "locked".to_string(),
        burn_on,
        recovery_step: None,
        sender_url: None,
        transport: sp_core::playback::TransportState::Idle,
    }
}

async fn ndi_health_json(state: crate::AppState) -> serde_json::Value {
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
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn ndi_health_exposes_burn_on_key_false_by_default() {
    let state = test_state().await;
    state
        .ndi_health_registry
        .update(burn_health_snapshot(false));
    let v = ndi_health_json(state).await;
    let arr = v.as_array().expect("response must be a JSON array");
    assert_eq!(
        arr[0]["burn_on"],
        serde_json::json!(false),
        "the health snapshot must carry burn_on and default it to false"
    );
}

#[tokio::test]
async fn ndi_health_exposes_burn_on_true_when_set() {
    let state = test_state().await;
    state.ndi_health_registry.update(burn_health_snapshot(true));
    let v = ndi_health_json(state).await;
    let arr = v.as_array().expect("response must be a JSON array");
    assert_eq!(
        arr[0]["burn_on"],
        serde_json::json!(true),
        "burn_on=true must serialise for the fleet's burn-leak guard"
    );
}
