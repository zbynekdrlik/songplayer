//! Translation-gender endpoint tests for `api::routes` (#152) — the per-song
//! ♂/♀ override (`PATCH /api/v1/lyrics/songs/{id}/translation-gender`) plus
//! the `translation_gender` field on the lyrics-songs list. Split out of
//! `routes_tests.rs` to keep it under the 1000-line airuleset cap. Included
//! as a sibling file via `#[path = "routes_tests_translation.rs"]
//! #[cfg(test)] mod tests_translation;` from routes.rs; shares
//! `test_state`/`app` with `routes_tests.rs` via `super::tests`.

#![allow(unused_imports)]

use super::tests::{app, test_state};
use super::*;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

async fn seed_video(pool: &sqlx::SqlitePool) {
    sqlx::query("INSERT INTO playlists (id, name, youtube_url, is_active) VALUES (1, 'p', 'u', 1)")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, has_lyrics, \
         lyrics_translation_version) VALUES (70, 1, 'ytGENDER', 1, 1, 1)",
    )
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn translation_gender_endpoint_sets_column_and_resets_version() {
    let state = test_state().await;
    seed_video(&state.pool).await;

    let resp = app(state.clone())
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/v1/lyrics/songs/70/translation-gender")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"gender": "f"})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let gender: Option<String> =
        sqlx::query_scalar("SELECT lyrics_translation_gender FROM videos WHERE id = 70")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    let version: i64 =
        sqlx::query_scalar("SELECT lyrics_translation_version FROM videos WHERE id = 70")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(gender, Some("f".to_string()), "gender column must be set");
    assert_eq!(
        version, 0,
        "setting the gender resets the translation version to re-translate the song"
    );
}

#[tokio::test]
async fn translation_gender_endpoint_null_clears_to_auto() {
    let state = test_state().await;
    seed_video(&state.pool).await;
    // First set to 'm', then clear.
    sqlx::query("UPDATE videos SET lyrics_translation_gender = 'm' WHERE id = 70")
        .execute(&state.pool)
        .await
        .unwrap();

    let resp = app(state.clone())
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/v1/lyrics/songs/70/translation-gender")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({ "gender": null })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let gender: Option<String> =
        sqlx::query_scalar("SELECT lyrics_translation_gender FROM videos WHERE id = 70")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(gender, None, "null gender clears the override to auto");
}

#[tokio::test]
async fn translation_gender_endpoint_returns_404_for_missing_video() {
    let state = test_state().await;
    let resp = app(state)
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/v1/lyrics/songs/999/translation-gender")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"gender": "m"})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn translation_gender_endpoint_rejects_bad_value() {
    let state = test_state().await;
    seed_video(&state.pool).await;
    let resp = app(state)
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/v1/lyrics/songs/70/translation-gender")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"gender": "x"})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn list_lyrics_songs_exposes_translation_gender() {
    let state = test_state().await;
    sqlx::query("INSERT INTO playlists (id, name, youtube_url, is_active) VALUES (1, 'p', 'u', 1)")
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, lyrics_translation_gender) \
         VALUES (1, 1, 'yt-m', 1, 'm'), (2, 1, 'yt-auto', 1, NULL)",
    )
    .execute(&state.pool)
    .await
    .unwrap();

    let resp = app(state)
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

    let male = items.iter().find(|v| v["video_id"] == 1).unwrap();
    let auto = items.iter().find(|v| v["video_id"] == 2).unwrap();
    assert_eq!(male["translation_gender"], serde_json::json!("m"));
    assert_eq!(auto["translation_gender"], serde_json::Value::Null);
}
