//! `GET /api/v1/ndi/health` clock-field exposure test (#146), and the #229
//! `open_failures` field. Split out of
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
        frames_submitted_total: 10,
        frames_submitted_last_5s: 3,
        observed_fps: 29.97,
        nominal_fps: 29.97,
        source_fps: 29.97,
        last_submit_ts: None,
        last_heartbeat_ts: None,
        consecutive_bad_polls: 0,
        degraded_reason: None,
        clock,
        pacing: Default::default(),
        audio: Default::default(),
        // #149 Lane 1: clock is NANO/ok but pacing is off in this fixture → UNLOCKED.
        lock_state: sp_core::genlock::lock_state::LockState::Unlocked,
        lock_reason: "pacing disabled".to_string(),
        transport: sp_core::playback::TransportState::Idle,
        open_failures: None,
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
    // #229: no failed open since the last start: the key is there, `null`.
    assert!(arr[0].get("open_failures").is_some_and(|v| v.is_null()));
}

/// #229: a playlist whose videos cannot be opened says so on the endpoint:
/// how many failed in a row, the last error, and when the next attempt is
/// due (UTC ms), so a black program has a visible reason.
#[tokio::test]
async fn ndi_health_endpoint_includes_open_failures() {
    use crate::playback::ndi_health::{PipelineHealthSnapshot, PlaybackStateLabel};

    let state = test_state().await;
    state.ndi_health_registry.update(PipelineHealthSnapshot {
        playlist_id: 4,
        ndi_name: "SP-slow".to_string(),
        state: PlaybackStateLabel::WaitingForScene,
        frames_submitted_total: 0,
        frames_submitted_last_5s: 0,
        observed_fps: 0.0,
        nominal_fps: 30.0,
        source_fps: 30.0,
        last_submit_ts: None,
        last_heartbeat_ts: None,
        consecutive_bad_polls: 0,
        degraded_reason: None,
        clock: Default::default(),
        pacing: Default::default(),
        audio: Default::default(),
        lock_state: sp_core::genlock::lock_state::LockState::Unlocked,
        lock_reason: "pacing disabled".to_string(),
        transport: sp_core::playback::TransportState::Idle,
        open_failures: Some(sp_core::playback::OpenFailures {
            count: 4,
            last_error: "No video: SetCurrentMediaType failed: No suitable transform".into(),
            retry_at_ms: Some(1_791_331_200_000),
        }),
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
    assert_eq!(
        v[0]["open_failures"],
        serde_json::json!({
            "count": 4,
            "last_error": "No video: SetCurrentMediaType failed: No suitable transform",
            "retry_at_ms": 1_791_331_200_000_i64,
        })
    );
}
