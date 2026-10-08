//! #233: `GET /api/v1/audio/asio-drivers` through the real router.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::api::routes::tests::{app, test_state};

#[tokio::test]
async fn the_driver_list_answers_a_list() {
    let state = test_state().await;
    let req = Request::builder()
        .uri("/api/v1/audio/asio-drivers")
        .body(Body::empty())
        .unwrap();
    let resp = app(state).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(json["drivers"].is_array());
    #[cfg(not(windows))]
    assert_eq!(
        json["drivers"],
        serde_json::json!([]),
        "no ASIO off Windows"
    );
}
