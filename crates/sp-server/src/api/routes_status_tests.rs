//! #230: `GET /api/v1/background-hold` through the real router.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::api::routes::tests::test_state;

async fn get_hold(state: crate::AppState) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .uri("/api/v1/background-hold")
        .body(Body::empty())
        .unwrap();
    let resp = crate::api::router(state, None).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn the_route_reports_no_hold_then_the_armed_one() {
    let state = test_state().await;
    let (status, json) = get_hold(state.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json,
        serde_json::json!({
            "held": false,
            "until_utc_ms": null,
            "remaining_s": 0,
            "hold_scene": "sp-90s",
            "release_scene": "sp-slow",
            "held_jobs": [],
        })
    );
    crate::background_hold::hold_for_a_minute(&state.pool).await;
    let (status, json) = get_hold(state).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["held"], true);
    assert!(json["until_utc_ms"].as_i64().is_some_and(|ms| ms > 0));
    let left = json["remaining_s"].as_u64().unwrap();
    assert!((59..=60).contains(&left), "{left}");
}
