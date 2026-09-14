//! ★ reference marker tests for `api::routes` (#142) — `lyrics_reference`
//! field exposure on the lyrics-songs list, plus the feedback and
//! admin-toggle endpoints. Split out of `routes_tests.rs` to keep it under
//! the 1000-line airuleset cap. Included as a sibling file via
//! `#[path = "routes_tests_reference.rs"] #[cfg(test)] mod tests_reference;`
//! from routes.rs; shares `test_state`/`app` with `routes_tests.rs` via
//! `super::tests`.

#![allow(unused_imports)]

use super::tests::{app, test_state};
use super::*;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

#[tokio::test]
async fn list_lyrics_songs_exposes_lyrics_reference() {
    // #142: the dashboard's ★ badge reads this field from the lyrics-songs
    // response to decide which rows carry Claude's verified reference lyrics.
    let state = test_state().await;
    sqlx::query("INSERT INTO playlists (id, name, youtube_url, is_active) VALUES (1, 'p', 'u', 1)")
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, lyrics_reference) \
         VALUES (1, 1, 'yt-ref', 1, 1), (2, 1, 'yt-noref', 1, 0)",
    )
    .execute(&state.pool)
    .await
    .unwrap();

    let app = app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/lyrics/songs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let items: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(items.len(), 2);

    let referenced = items
        .iter()
        .find(|v| v["video_id"] == 1)
        .expect("row for video 1");
    let not_referenced = items
        .iter()
        .find(|v| v["video_id"] == 2)
        .expect("row for video 2");
    assert_eq!(
        referenced["lyrics_reference"],
        serde_json::Value::Bool(true),
        "video 1 must serialize lyrics_reference=true"
    );
    assert_eq!(
        not_referenced["lyrics_reference"],
        serde_json::Value::Bool(false),
        "video 2 must serialize lyrics_reference=false"
    );
}

// ── #142 — ★ reference marker: feedback + admin toggle endpoints ──────────

#[tokio::test]
async fn reference_feedback_endpoint_clears_flag_and_records_note() {
    let state = test_state().await;
    sqlx::query("INSERT INTO playlists (id, name, youtube_url, is_active) VALUES (1, 'p', 'u', 1)")
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, lyrics_reference) \
         VALUES (50, 1, 'ytREF', 1, 1)",
    )
    .execute(&state.pool)
    .await
    .unwrap();

    let resp = app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/lyrics/songs/50/reference-feedback")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"note": "refrén nesedí s videom"}))
                        .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let row = sqlx::query(
        "SELECT lyrics_reference, lyrics_reference_rejected_at, lyrics_reference_note, \
         lyrics_manual_priority FROM videos WHERE id = 50",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let reference: i64 = row.get("lyrics_reference");
    let rejected_at: Option<String> = row.get("lyrics_reference_rejected_at");
    let note: Option<String> = row.get("lyrics_reference_note");
    let manual_priority: i64 = row.get("lyrics_manual_priority");
    assert_eq!(reference, 0, "star must be cleared");
    assert!(rejected_at.is_some(), "rejected_at must be stamped");
    assert_eq!(note, Some("refrén nesedí s videom".to_string()));
    assert_eq!(manual_priority, 1, "song must be re-queued");
}

#[tokio::test]
async fn reference_feedback_endpoint_returns_404_for_missing_video() {
    let state = test_state().await;
    let resp = app(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/lyrics/songs/999/reference-feedback")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"note": "x"})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn set_reference_endpoint_toggles_flag_true_and_false() {
    let state = test_state().await;
    sqlx::query("INSERT INTO playlists (id, name, youtube_url, is_active) VALUES (1, 'p', 'u', 1)")
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized) VALUES (60, 1, 'ytSET', 1)",
    )
    .execute(&state.pool)
    .await
    .unwrap();

    let resp = app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/lyrics/songs/60/reference")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"reference": true})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let on: i64 = sqlx::query_scalar("SELECT lyrics_reference FROM videos WHERE id = 60")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(on, 1);

    let resp = app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/lyrics/songs/60/reference")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"reference": false})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let off: i64 = sqlx::query_scalar("SELECT lyrics_reference FROM videos WHERE id = 60")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(off, 0);
}

#[tokio::test]
async fn set_reference_endpoint_returns_404_for_missing_video() {
    let state = test_state().await;
    let resp = app(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/lyrics/songs/999/reference")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"reference": true})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
