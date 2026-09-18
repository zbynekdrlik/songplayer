//! Tests for the Dabing API (#180): pure request-body parsing + axum
//! integration for the toggle / list / mixer routes. Included from
//! `api/dabing.rs` via `#[path]`; reuses the shared `routes::tests` harness.

#![allow(unused_imports)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use super::{DabingImportReq, DubMixReq, DubToggleReq};
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

#[test]
fn parses_dub_mix_request() {
    let req: DubMixReq = serde_json::from_str(r#"{"ratio":0.35}"#).unwrap();
    assert!((req.ratio - 0.35).abs() < f64::EPSILON);
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
    crate::startup::ensure_dabing_playlist_exists(&pool)
        .await
        .unwrap();
    let vid = seed_video(&pool, 900, "dab1").await;
    crate::db::models_dabing::set_dub_requested(&pool, vid, true)
        .await
        .unwrap();

    let json = get_json(app(state), "/api/v1/dabing").await;
    assert!(
        json["playlist_id"].as_i64().is_some(),
        "the seeded Dabing playlist id must be present"
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

#[tokio::test]
async fn patch_dub_mix_clamps_and_persists() {
    let state = test_state().await;
    let pool = state.pool.clone();
    let vid = seed_video(&pool, 902, "dab3").await;

    let resp = app(state.clone())
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/videos/{vid}/dub-mix"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"ratio":1.9}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["ratio"].as_f64(),
        Some(1.0),
        "over-range ratio clamps to 1.0"
    );

    let stored: f64 = sqlx::query_scalar("SELECT dub_mix_ratio FROM videos WHERE id = ?")
        .bind(vid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, 1.0);

    // Unknown id → 404 (consistent with the dub toggle).
    let resp = app(state)
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/v1/videos/999999/dub-mix")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"ratio":0.5}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
