//! #233: `GET /api/v1/audio/asio-drivers` through the real router.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use super::drivers_answer;
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

async fn body_of(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// #233 release review: a read answers its drivers.
#[tokio::test]
async fn a_read_driver_list_answers_200_with_its_drivers() {
    let resp = drivers_answer(Ok(vec!["Dante Virtual Soundcard (x64)".into()]));
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_of(resp).await,
        r#"{"drivers":["Dante Virtual Soundcard (x64)"]}"#
    );
}

/// #233 release review: a list that could not be read is a 500 naming why,
/// never an empty list the dashboard would take for "no driver".
#[tokio::test]
async fn an_unreadable_driver_list_is_a_500_naming_why() {
    let resp = drivers_answer(Err("access denied".into()));
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body_of(resp).await,
        "the ASIO driver list could not be read: access denied"
    );
}
