//! `PATCH /api/v1/videos/{id}` song + artist correction tests (#136 T1).
//! Included via `#[path] #[cfg(test)] mod tests_patch_metadata;` from
//! routes.rs; shares `test_state`/`app` with `routes_tests.rs` via
//! `super::tests`. Kept in its own sibling file so `routes_tests.rs` stays
//! under the 1000-line file-size gate.
//!
//! Operators correct wall metadata that the Gemini-failed regex fallback
//! wrote wrong (swapped / initial-shortened song+artist, mojibake) — the
//! deterministic R4-rejected lever. The handler must sanitize through the
//! central `metadata::sanitize::strip_emoji` choke point and reject a
//! whitespace-only `song` with 400.

#![allow(unused_imports)]

use super::tests::{app, test_state};
use super::*;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

/// Seed one playlist + one video with the given song/artist; the caller
/// clones `state.pool` for post-PATCH assertions.
async fn seed_video(state: &crate::AppState, id: i64, song: &str, artist: &str) {
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p', 'u')")
        .execute(&state.pool)
        .await
        .ok(); // playlist may already exist across multi-seed tests
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, song, artist) \
         VALUES (?, 1, ?, 1, ?, ?)",
    )
    .bind(id)
    .bind(format!("yt-{id}"))
    .bind(song)
    .bind(artist)
    .execute(&state.pool)
    .await
    .unwrap();
}

async fn patch(state: crate::AppState, id: i64, body: serde_json::Value) -> StatusCode {
    app(state)
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/videos/{id}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// The core lever: PATCH song + artist writes both columns and replies 204.
#[tokio::test]
async fn patch_video_sets_song_and_artist() {
    let state = test_state().await;
    // A Gemini-failed swap: artist holds the initialized band, song the wrong half.
    seed_video(&state, 10, "planetboom", "P. Break!").await;
    let pool = state.pool.clone();

    let status = patch(
        state,
        10,
        serde_json::json!({ "song": "Break!", "artist": "planetboom" }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (song, artist): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT song, artist FROM videos WHERE id = 10")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        song.as_deref(),
        Some("Break!"),
        "song column must be corrected"
    );
    assert_eq!(
        artist.as_deref(),
        Some("planetboom"),
        "artist column must be corrected"
    );
}

/// A whitespace-only `song` is a clear operator error — reject with 400 and
/// leave the stored song untouched.
#[tokio::test]
async fn patch_video_rejects_whitespace_only_song() {
    let state = test_state().await;
    seed_video(&state, 11, "Real Song", "Real Artist").await;
    let pool = state.pool.clone();

    let status = patch(
        state,
        11,
        serde_json::json!({ "song": "   ", "artist": "Real Artist" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let song: Option<String> = sqlx::query_scalar("SELECT song FROM videos WHERE id = 11")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        song.as_deref(),
        Some("Real Song"),
        "a rejected whitespace song must not overwrite the stored song"
    );
}

/// Operator input runs through the same central sanitizer as ingested
/// titles — emoji / high-plane junk is stripped before it reaches the DB.
#[tokio::test]
async fn patch_video_sanitizes_emoji_from_song_and_artist() {
    let state = test_state().await;
    seed_video(&state, 12, "old", "old").await;
    let pool = state.pool.clone();

    let status = patch(
        state,
        12,
        serde_json::json!({ "song": "Way Maker \u{1F525}", "artist": "Sinach \u{2728}" }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (song, artist): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT song, artist FROM videos WHERE id = 12")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        song.as_deref(),
        Some("Way Maker"),
        "emoji stripped from song"
    );
    assert_eq!(
        artist.as_deref(),
        Some("Sinach"),
        "emoji stripped from artist"
    );
}

/// An empty artist clears the column to NULL (some songs have no artist),
/// mirroring the `lyrics_override_text` empty->NULL convention.
#[tokio::test]
async fn patch_video_clears_artist_to_null_on_empty() {
    let state = test_state().await;
    seed_video(&state, 13, "Song", "Delete Me").await;
    let pool = state.pool.clone();

    let status = patch(state, 13, serde_json::json!({ "artist": "" })).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let artist: Option<String> = sqlx::query_scalar("SELECT artist FROM videos WHERE id = 13")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(artist, None, "empty artist must clear to NULL");
}

/// A song-only PATCH must not clobber the untouched artist column (dynamic
/// UPDATE touches only provided fields).
#[tokio::test]
async fn patch_video_song_only_leaves_artist_untouched() {
    let state = test_state().await;
    seed_video(&state, 14, "Wrong", "Keep This Artist").await;
    let pool = state.pool.clone();

    let status = patch(state, 14, serde_json::json!({ "song": "Right" })).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (song, artist): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT song, artist FROM videos WHERE id = 14")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(song.as_deref(), Some("Right"));
    assert_eq!(
        artist.as_deref(),
        Some("Keep This Artist"),
        "artist must survive a song-only PATCH"
    );
}
