//! `GET /api/v1/ndi/health` pacing-field exposure test (#147). Included via
//! `#[path = "routes_tests_pacing.rs"] #[cfg(test)] mod tests_pacing;` from
//! routes.rs; shares `test_state`/`app` with `routes_tests.rs` via
//! `super::tests`. Mirrors the #146 clock exposure test.

#![allow(unused_imports)]

use super::tests::{app, test_state};
use super::*;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

#[tokio::test]
async fn ndi_health_endpoint_includes_pacing() {
    use crate::playback::ndi_health::{PacingStats, PipelineHealthSnapshot, PlaybackStateLabel};

    let state = test_state().await;
    state.ndi_health_registry.update(PipelineHealthSnapshot {
        playlist_id: 22,
        ndi_name: "SP-paced".to_string(),
        state: PlaybackStateLabel::Playing,
        connections: 1,
        frames_submitted_total: 10,
        frames_submitted_last_5s: 3,
        observed_fps: 30.0,
        nominal_fps: 30.0,
        last_submit_ts: None,
        last_heartbeat_ts: None,
        consecutive_bad_polls: 0,
        degraded_reason: None,
        clock: Default::default(),
        pacing: PacingStats {
            enabled: true,
            seq: 42,
            late_frames: 1,
            max_late_us: 900,
            jitter_p99_us: 120,
            repeats: 2,
            resyncs: 0,
            relatches: 0,
            dropped: 30,
            lag_slots: 3,
            iter_p99_us: 4200,
        },
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
    assert_eq!(arr.len(), 1, "exactly one seeded pipeline");
    assert_eq!(
        arr[0]["pacing"]["enabled"],
        serde_json::json!(true),
        "the health snapshot must carry pacing.enabled"
    );
    assert_eq!(arr[0]["pacing"]["seq"].as_u64(), Some(42));
    assert_eq!(arr[0]["pacing"]["dropped"].as_u64(), Some(30));
    assert_eq!(arr[0]["pacing"]["lag_slots"].as_i64(), Some(3));
    assert_eq!(arr[0]["pacing"]["iter_p99_us"].as_u64(), Some(4200));
}
