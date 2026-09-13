//! `GET /api/v1/ndi/health` clock-field exposure test (#146). Split out of
//! `routes_tests.rs` to keep it under the 1000-line airuleset cap. Included
//! via `#[path = "routes_tests_clock.rs"] #[cfg(test)] mod tests_clock;`
//! from routes.rs; shares `test_state`/`app` with `routes_tests.rs` via
//! `super::tests`.

#![allow(unused_imports)]

use super::tests::{app, test_state};
use super::*;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

#[tokio::test]
async fn ndi_health_endpoint_includes_clock() {
    use crate::playback::clock_health::{DantesyncStatus, evaluate};
    use crate::playback::ndi_health::{PipelineHealthSnapshot, PlaybackStateLabel};

    let state = test_state().await;
    let clock = evaluate(Some(&DantesyncStatus {
        is_locked: Some(true),
        mode: Some("NANO".to_string()),
        offset_ns: Some(164_707),
        ntp_failed: Some(false),
        ntp_age_s: Some(37),
    }));
    state.ndi_health_registry.update(PipelineHealthSnapshot {
        playlist_id: 21,
        ndi_name: "SP-clock".to_string(),
        state: PlaybackStateLabel::Playing,
        connections: 1,
        frames_submitted_total: 10,
        frames_submitted_last_5s: 3,
        observed_fps: 29.97,
        nominal_fps: 29.97,
        last_submit_ts: None,
        last_heartbeat_ts: None,
        consecutive_bad_polls: 0,
        degraded_reason: None,
        clock,
        pacing: Default::default(),
        audio: Default::default(),
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
        arr[0]["clock"]["clock_ok"],
        serde_json::json!(true),
        "the health snapshot must carry clock.clock_ok"
    );
    assert_eq!(arr[0]["clock"]["mode"], serde_json::json!("NANO"));
    assert_eq!(arr[0]["clock"]["offset_ns"].as_i64(), Some(164_707));
}
