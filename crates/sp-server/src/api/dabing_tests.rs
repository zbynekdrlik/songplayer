//! Tests for the Dabing API (#180): pure request-body parsing + axum
//! integration for the toggle / list / mixer routes. Included from
//! `api/dabing.rs` via `#[path]`; reuses the shared `routes::tests` harness.

#![allow(unused_imports)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use super::{DabingImportReq, DubToggleReq};
use crate::api::routes::tests::{app, test_state};

// ── pure request parsing ──────────────────────────────────────────────────

#[test]
fn parses_import_request() {
    let req: DabingImportReq =
        serde_json::from_str(r#"{"url":"https://youtu.be/AvWOCj48pGw"}"#).unwrap();
    assert_eq!(req.url, "https://youtu.be/AvWOCj48pGw");
}

#[test]
fn parses_dub_toggle_request() {
    let on: DubToggleReq = serde_json::from_str(r#"{"requested":true}"#).unwrap();
    assert!(on.requested);
    let off: DubToggleReq = serde_json::from_str(r#"{"requested":false}"#).unwrap();
    assert!(!off.requested);
}

// ── axum integration ──────────────────────────────────────────────────────

async fn seed_video(pool: &sqlx::SqlitePool, playlist_id: i64, youtube_id: &str) -> i64 {
    sqlx::query("INSERT INTO playlists (id, name, youtube_url, is_active) VALUES (?, 'p', 'u', 1)")
        .bind(playlist_id)
        .execute(pool)
        .await
        .ok();
    sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, title) VALUES (?, ?, ?) RETURNING id",
    )
    .bind(playlist_id)
    .bind(youtube_id)
    .bind(format!("Song {youtube_id}"))
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
async fn get_dabing_returns_seeded_playlist_and_requested_videos() {
    let state = test_state().await;
    let pool = state.pool.clone();
    // Insert an unrelated playlist FIRST so the Dabing playlist does NOT get
    // id 1 — this makes the returned playlist_id assertion kill a mutant that
    // hardcodes Some(1)/Some(0)/Some(-1) for dabing_playlist_id.
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'other', '')")
        .execute(&pool)
        .await
        .unwrap();
    crate::startup::ensure_dabing_playlist_exists(&pool)
        .await
        .unwrap();
    // The exact id the seed created — the response must return THIS, not a
    // constant.
    let real_id: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE kind = 'dabing'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_ne!(real_id, 1, "Dabing playlist must not be id 1 in this test");
    let vid = seed_video(&pool, 900, "dab1").await;
    crate::db::models_dabing::set_dub_requested(&pool, vid, true)
        .await
        .unwrap();

    let json = get_json(app(state), "/api/v1/dabing").await;
    assert_eq!(
        json["playlist_id"].as_i64(),
        Some(real_id),
        "the response must return the ACTUAL seeded Dabing playlist id"
    );
    let videos = json["videos"].as_array().unwrap();
    assert_eq!(videos.len(), 1);
    assert_eq!(videos[0]["video_id"].as_i64(), Some(vid));
    assert_eq!(videos[0]["dub_status"].as_str(), Some("queued"));
    assert_eq!(videos[0]["chain_state"].as_str(), Some("queued"));
}

#[tokio::test]
async fn patch_dub_toggles_request_flag() {
    let state = test_state().await;
    let pool = state.pool.clone();
    let vid = seed_video(&pool, 901, "dab2").await;

    let resp = app(state.clone())
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/videos/{vid}/dub"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"requested":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let requested: i64 = sqlx::query_scalar("SELECT dub_requested FROM videos WHERE id = ?")
        .bind(vid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(requested, 1, "PATCH must flip dub_requested on");

    // Unknown id → 404.
    let resp = app(state)
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/v1/videos/999999/dub")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"requested":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
