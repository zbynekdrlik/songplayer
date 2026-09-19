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
    use crate::playback::ndi_health::{
        AudioStats, PacingStats, PipelineHealthSnapshot, PlaybackStateLabel,
    };

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
            prep_p99_us: 210,
        },
        audio: AudioStats {
            enabled: true,
            residual_ppm: -12.5,
            applied_ppm: 8.0,
            samples_per_boundary: 1600,
            underruns: 4,
            overflows: 1,
            buffer_ms: 66,
            emitter: Default::default(),
        },
        // #149 Lane 1: an enabled, receiver-connected, event-free pipeline is LOCKED.
        lock_state: sp_core::genlock::lock_state::LockState::Locked,
        lock_reason: "locked".to_string(),
        burn_on: false,
        recovery_step: None,
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
    // #147 lane 4: the pre-decode duration gauge serialises on the endpoint too.
    assert_eq!(arr[0]["pacing"]["prep_p99_us"].as_u64(), Some(210));

    // #148: the audio clock-discipline telemetry serialises with its full key set.
    assert_eq!(
        arr[0]["audio"]["enabled"],
        serde_json::json!(true),
        "the health snapshot must carry audio.enabled"
    );
    assert_eq!(arr[0]["audio"]["samples_per_boundary"].as_u64(), Some(1600));
    assert_eq!(arr[0]["audio"]["underruns"].as_u64(), Some(4));
    assert_eq!(arr[0]["audio"]["overflows"].as_u64(), Some(1));
    assert_eq!(arr[0]["audio"]["buffer_ms"].as_u64(), Some(66));
    assert_eq!(arr[0]["audio"]["applied_ppm"].as_f64(), Some(8.0));
    assert!(
        arr[0]["audio"]["residual_ppm"].as_f64().is_some(),
        "audio.residual_ppm must serialise"
    );

    // #149 Lane 1: lock_state/lock_reason serialise; this snapshot is LOCKED.
    assert_eq!(arr[0]["lock_state"].as_str(), Some("LOCKED"));
    assert_eq!(arr[0]["lock_reason"].as_str(), Some("locked"));
}
