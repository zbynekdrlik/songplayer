//! #223 S11: the route's refusals through the real router (an upgrade
//! itself needs yt-dlp and Media Foundation: `video_upgrade`'s tests script
//! its steps).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::api::routes::tests::{app, test_state};

async fn post(body: &str) -> (StatusCode, serde_json::Value) {
    let resp = app(test_state().await)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/video-upgrade")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn a_value_that_is_not_a_youtube_id_is_refused() {
    for bad in [
        "",
        "PySFfTuraf",
        "PySFfTurafAA",
        "../../etc/p",
        "PySFfTur fA",
    ] {
        let (status, body) = post(&serde_json::json!({ "youtube_id": bad }).to_string()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad:?}");
        assert_eq!(body["error"], "youtube_id must be a YouTube id", "{bad:?}");
    }
}

/// A YouTube id (trimmed) gets past the id check: off Windows the reader
/// is missing (501); on the Windows job the tools are not ready in a test
/// state (503).
#[tokio::test]
async fn a_youtube_id_needs_the_reader_and_the_tools() {
    let (status, body) = post(r#"{"youtube_id":" PySFfTurafA "}"#).await;
    if cfg!(windows) {
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"], "yt-dlp is not ready yet on this server");
    } else {
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(body["error"], "the video reader is Windows only");
    }
}
