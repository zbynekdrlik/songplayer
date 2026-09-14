//! `GET /api/v1/playback/{id}/preview.jpg` route tests (#15 part 2).
//! Included via `#[path] #[cfg(test)] mod tests;` from `api/preview.rs`;
//! reuses the shared `app`/`test_state` harness from `routes_tests.rs`.

use crate::AppState;
use crate::api::routes::tests::{app, test_state};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

struct PreviewResponse {
    status: StatusCode,
    content_type: Option<String>,
    body: Vec<u8>,
}

async fn get_preview(state: AppState, playlist_id: i64) -> PreviewResponse {
    let resp = app(state)
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/playback/{playlist_id}/preview.jpg"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    PreviewResponse {
        status,
        content_type,
        body,
    }
}

#[tokio::test]
async fn preview_unknown_playlist_returns_404() {
    let state = test_state().await;
    let r = get_preview(state, 4242).await;
    assert_eq!(
        r.status,
        StatusCode::NOT_FOUND,
        "no pipeline for this playlist → 404"
    );
}

#[tokio::test]
async fn preview_registered_but_idle_returns_204() {
    let state = test_state().await;
    // A registered pipeline that has produced no preview frame yet.
    state.preview_registry.register(1);
    let r = get_preview(state, 1).await;
    assert_eq!(
        r.status,
        StatusCode::NO_CONTENT,
        "registered but no frame yet → 204"
    );
    assert!(r.body.is_empty(), "204 carries no body");
}

#[tokio::test]
async fn preview_returns_jpeg_when_a_frame_is_available() {
    let state = test_state().await;
    let tap = state.preview_registry.register(2);
    // Seed a published JPEG (a real minimal JPEG byte sequence).
    let seeded = vec![0xFF, 0xD8, 0xFF, 0xD9];
    tap.set_latest_jpeg_for_test(seeded.clone());

    let r = get_preview(state, 2).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        r.content_type.as_deref(),
        Some("image/jpeg"),
        "preview must be served as image/jpeg"
    );
    assert_eq!(
        r.body, seeded,
        "the exact published JPEG bytes are returned"
    );
}
