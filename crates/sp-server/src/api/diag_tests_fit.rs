//! Router tests of `POST /api/v1/diag/fit-bench` (#223 S10a), through the
//! real router. The fit runs on every platform (pure CPU), so a small run is
//! real here; the 4K numbers are the box's.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::AppState;
use crate::api::routes::tests::test_state;
use crate::diag::decode_bench::DecodeBench;

/// A state with its own bench gate, so the 409 test cannot race another.
async fn state() -> AppState {
    let mut state = test_state().await;
    let dir = std::env::temp_dir().join("sp-fit-bench-unused");
    state.decode_bench = Arc::new(DecodeBench::new(dir));
    state
}

async fn post(state: &AppState, body: serde_json::Value) -> (StatusCode, String) {
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/diag/fit-bench")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = crate::api::router(state.clone(), None)
        .oneshot(req)
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn a_size_out_of_range_is_400_naming_the_rule() {
    let state = state().await;
    let (status, text) = post(
        &state,
        serde_json::json!({"width": 1919, "height": 1080, "frames": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    assert_eq!(text, "width and height must be even (NV12)");
}

/// One bench at a time: a held slot (the decode bench's) is 409; once free,
/// a small run answers its report and leaves the bench free.
#[tokio::test]
async fn a_held_bench_is_409_then_a_run_reports_and_frees_it() {
    let state = state().await;
    let body = serde_json::json!({"width": 64, "height": 36, "frames": 2});
    let slot = state.decode_bench.try_start().expect("the bench is free");
    let (status, text) = post(&state, body.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{text}");
    assert_eq!(text, "a bench run is in progress");
    drop(slot);
    let (status, text) = post(&state, body).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let report: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(report["width"], 64);
    assert_eq!(report["frames"], 2);
    assert_eq!(report["canvas_width"], 1920);
    assert_eq!(report["budget_us"], 16_666);
    assert!(
        state.decode_bench.try_start().is_some(),
        "the run freed the bench"
    );
}
