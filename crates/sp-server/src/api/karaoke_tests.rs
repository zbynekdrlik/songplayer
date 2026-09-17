//! Axum integration tests (#177) for the karaoke now-playing payload, the
//! videos-list `stems_state` marker, and the stem re-enqueue endpoint. Included
//! from `api/karaoke.rs` via `#[path]` so the handler file stays lean. Reuses
//! the shared `routes::tests` AppState + router harness.

#![allow(unused_imports)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::api::routes::tests::{app, test_state};

async fn insert_playlist(pool: &sqlx::SqlitePool, id: i64) {
    sqlx::query("INSERT INTO playlists (id, name, youtube_url, is_active) VALUES (?, 'p', 'u', 1)")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

/// Insert a normalized, stem-eligible video and return its id.
async fn insert_video(pool: &sqlx::SqlitePool, playlist_id: i64, youtube_id: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, title, normalized, file_path, audio_file_path) \
         VALUES (?, ?, ?, 1, ?, ?) RETURNING id",
    )
    .bind(playlist_id)
    .bind(youtube_id)
    .bind(format!("Song {youtube_id}"))
    .bind(format!("/c/{youtube_id}_video.mp4"))
    .bind(format!("/c/{youtube_id}_audio.flac"))
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn get_json(app: axum::Router, uri: &str) -> serde_json::Value {
    let resp = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "GET {uri}");
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn karaoke_now_playing_carries_per_song_stems_state() {
    let state = test_state().await;
    let pool = state.pool.clone();
    // A distinct playlist id so this test's now-playing entry is isolated from
    // any set by a parallel test (the registry is process-global).
    insert_playlist(&pool, 771).await;
    let vid = insert_video(&pool, 771, "np_ready").await;
    // Both stems present → Ready.
    crate::db::models_stems::mark_stems_done(&pool, vid, "/c/v.flac", "/c/i.flac")
        .await
        .unwrap();
    crate::now_playing::global().set(771, vid);

    let json = get_json(app(state), "/api/v1/karaoke").await;
    let np = json["now_playing"].as_array().expect("now_playing array");
    let entry = np
        .iter()
        .find(|e| e["playlist_id"] == 771)
        .expect("entry for playlist 771");
    assert_eq!(entry["video_id"], vid);
    assert_eq!(entry["title"], "Song np_ready");
    assert_eq!(entry["stems_state"], "ready");
    assert!(entry["stems_error"].is_null());
    assert!(
        entry["queue_position"].is_null(),
        "ready song is not queued"
    );

    crate::now_playing::global().clear(771);
}

#[tokio::test]
async fn karaoke_now_playing_reports_queued_position_and_failed_error() {
    let state = test_state().await;
    let pool = state.pool.clone();
    insert_playlist(&pool, 772).await;
    let queued = insert_video(&pool, 772, "np_queued").await;
    crate::now_playing::global().set(772, queued);

    let json = get_json(app(state.clone()), "/api/v1/karaoke").await;
    let np = json["now_playing"].as_array().unwrap();
    let entry = np.iter().find(|e| e["playlist_id"] == 772).unwrap();
    assert_eq!(entry["stems_state"], "queued");
    assert_eq!(entry["queue_position"], 1);

    // Now fail it → state failed + a human error, no queue position.
    crate::db::models_stems::record_stem_deferral(
        &pool,
        queued,
        std::time::Duration::from_secs(600),
    )
    .await
    .unwrap();
    let json = get_json(app(state), "/api/v1/karaoke").await;
    let entry = json["now_playing"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["playlist_id"] == 772)
        .unwrap()
        .clone();
    assert_eq!(entry["stems_state"], "failed");
    assert!(entry["stems_error"].as_str().unwrap().contains("zlyhal"));
    assert!(entry["queue_position"].is_null());

    crate::now_playing::global().clear(772);
}

#[tokio::test]
async fn videos_payload_carries_stems_state_marker() {
    let state = test_state().await;
    let pool = state.pool.clone();
    insert_playlist(&pool, 773).await;
    let ready = insert_video(&pool, 773, "vl_ready").await;
    let queued = insert_video(&pool, 773, "vl_queued").await;
    let unsup = insert_video(&pool, 773, "vl_unsup").await;
    crate::db::models_stems::mark_stems_done(&pool, ready, "/c/v.flac", "/c/i.flac")
        .await
        .unwrap();
    crate::db::models_stems::mark_stems_unsupported(&pool, unsup)
        .await
        .unwrap();

    let json = get_json(app(state), "/api/v1/playlists/773/videos").await;
    let rows = json.as_array().unwrap();
    let state_of = |id: i64| {
        rows.iter()
            .find(|v| v["id"] == id)
            .and_then(|v| v["stems_state"].as_str())
            .map(str::to_string)
    };
    assert_eq!(state_of(ready).as_deref(), Some("ready"));
    assert_eq!(state_of(queued).as_deref(), Some("queued"));
    assert_eq!(state_of(unsup).as_deref(), Some("unavailable"));
}

#[tokio::test]
async fn enqueue_endpoint_reopens_a_failed_song() {
    let state = test_state().await;
    let pool = state.pool.clone();
    insert_playlist(&pool, 774).await;
    let vid = insert_video(&pool, 774, "eq").await;
    crate::db::models_stems::mark_stems_unsupported(&pool, vid)
        .await
        .unwrap();
    // Terminal → not selectable.
    assert!(
        crate::db::models_stems::get_next_video_for_stems(&pool)
            .await
            .unwrap()
            .is_none()
    );

    let resp = app(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/stems/{vid}/enqueue"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "enqueued");
    assert_eq!(json["queue_position"], 1);

    // Now selectable again.
    assert_eq!(
        crate::db::models_stems::get_next_video_for_stems(&pool)
            .await
            .unwrap()
            .map(|j| j.video_id),
        Some(vid)
    );
}
