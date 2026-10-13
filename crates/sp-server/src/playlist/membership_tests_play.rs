//! #260: a row whose song left its YouTube playlist is never picked to
//! play and never listed; a custom playlist's set list (an operator's
//! choice) is not touched by the mark.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sp_core::playback::PlaybackMode;
use sqlx::SqlitePool;
use tower::ServiceExt;

use crate::api::routes::tests::{app, test_state_with_engine_rx};
use crate::db::models;
use crate::playlist::loop_memory;
use crate::playlist::selector::VideoSelector;

/// A youtube playlist with `count` downloaded songs `yt_0`, `yt_1`, … on
/// `pool`; returns the playlist id and the row ids in order.
async fn songs(pool: &SqlitePool, count: usize) -> (i64, Vec<i64>) {
    let playlist = models::insert_playlist(pool, "fast", "https://yt.com/fast")
        .await
        .unwrap();
    let mut ids = Vec::new();
    for i in 0..count {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO videos (playlist_id, youtube_id, title, normalized, file_path, \
             audio_file_path) VALUES (?, ?, ?, 1, ?, ?) RETURNING id",
        )
        .bind(playlist.id)
        .bind(format!("yt_{i}"))
        .bind(format!("Song {i}"))
        .bind(format!("/cache/song_{i}_video.mp4"))
        .bind(format!("/cache/song_{i}_audio.flac"))
        .fetch_one(pool)
        .await
        .unwrap();
        ids.push(id);
    }
    (playlist.id, ids)
}

async fn memory_pool() -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    pool
}

/// The mark as the sync sets it (`models_membership::mark_removed`).
async fn mark(pool: &SqlitePool, id: i64) {
    sqlx::query("UPDATE videos SET removed_at = 1760000000000 WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

/// Thirty picks out of three songs, two of them marked: a random pick
/// cannot pass by luck.
#[tokio::test]
async fn continuous_never_picks_a_song_that_left_the_playlist() {
    let pool = memory_pool().await;
    let (playlist, ids) = songs(&pool, 3).await;
    mark(&pool, ids[0]).await;
    mark(&pool, ids[2]).await;
    for _ in 0..30 {
        let pick = VideoSelector::select_next(&pool, playlist, PlaybackMode::Continuous, None, &[])
            .await
            .unwrap();
        assert_eq!(pick, Some(ids[1]));
    }
}

/// The owner's case: a marked song is never played, so it stays the one
/// unplayed song of the rotation. The rotation starts over from the songs
/// still in the playlist instead of picking it.
#[tokio::test]
async fn a_rotation_starts_over_rather_than_pick_a_song_that_left() {
    let pool = memory_pool().await;
    let (playlist, ids) = songs(&pool, 3).await;
    models::record_play(&pool, playlist, ids[0]).await.unwrap();
    models::record_play(&pool, playlist, ids[1]).await.unwrap();
    mark(&pool, ids[2]).await;
    let pick = VideoSelector::select_next(&pool, playlist, PlaybackMode::Single, None, &[])
        .await
        .unwrap()
        .expect("a song still in the playlist");
    assert_ne!(pick, ids[2]);
    // The pick is recorded only once it starts: the history started over.
    assert_eq!(
        models::get_unplayed_video_ids(&pool, playlist)
            .await
            .unwrap(),
        vec!["yt_0".to_string(), "yt_1".to_string()]
    );
}

#[tokio::test]
async fn the_unplayed_and_the_downloaded_lists_leave_a_marked_row_out() {
    let pool = memory_pool().await;
    let (playlist, ids) = songs(&pool, 3).await;
    mark(&pool, ids[1]).await;
    assert_eq!(
        models::get_unplayed_video_ids(&pool, playlist)
            .await
            .unwrap(),
        vec!["yt_0".to_string(), "yt_2".to_string()]
    );
    assert_eq!(
        models::get_unplayed_normalized_video_ids(&pool, playlist)
            .await
            .unwrap(),
        vec![ids[0], ids[2]]
    );
    assert_eq!(
        models::get_normalized_video_ids(&pool, playlist)
            .await
            .unwrap(),
        vec![ids[0], ids[2]]
    );
}

/// After a restart Loop takes the song it looped, but not one that left
/// the playlist since: it picks one still in it.
#[tokio::test]
async fn loop_after_a_restart_skips_a_remembered_song_that_left() {
    let pool = memory_pool().await;
    let (playlist, ids) = songs(&pool, 3).await;
    loop_memory::remember(&pool, playlist, ids[1])
        .await
        .unwrap();
    assert_eq!(
        loop_memory::remembered(&pool, playlist).await.unwrap(),
        Some(ids[1])
    );
    mark(&pool, ids[1]).await;
    assert_eq!(
        loop_memory::remembered(&pool, playlist).await.unwrap(),
        None
    );
    for _ in 0..10 {
        let pick = VideoSelector::select_next(&pool, playlist, PlaybackMode::Loop, None, &[])
            .await
            .unwrap()
            .expect("a song still in the playlist");
        assert_ne!(pick, ids[1]);
    }
}

/// A custom playlist's set list is the operator's own choice: a song in it
/// plays whatever its YouTube playlist did (the mark is about that
/// playlist's rotation only).
#[tokio::test]
async fn a_custom_set_list_keeps_a_song_that_left_its_youtube_playlist() {
    let pool = memory_pool().await;
    let (_, ids) = songs(&pool, 2).await;
    let custom: i64 = sqlx::query_scalar(
        "INSERT INTO playlists (name, youtube_url, kind) VALUES ('live', '', 'custom') \
         RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    models::append_playlist_item(&pool, custom, ids[1])
        .await
        .unwrap();
    mark(&pool, ids[1]).await;
    loop_memory::remember(&pool, custom, ids[1]).await.unwrap();
    assert_eq!(
        loop_memory::remembered(&pool, custom).await.unwrap(),
        Some(ids[1])
    );
    let pick = VideoSelector::select_next(&pool, custom, PlaybackMode::Continuous, None, &[])
        .await
        .unwrap();
    assert_eq!(pick, Some(ids[1]));
}

/// The dashboard's list (`GET /api/v1/playlists/{id}/videos`, the real
/// router) shows only the songs in the YouTube playlist.
#[tokio::test]
async fn the_dashboard_lists_only_the_songs_in_the_playlist() {
    let (state, _engine_rx) = test_state_with_engine_rx().await;
    let (playlist, ids) = songs(&state.pool, 3).await;
    mark(&state.pool, ids[1]).await;
    let resp = app(state)
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/playlists/{playlist}/videos"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let rows: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let listed: Vec<i64> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_i64().unwrap())
        .collect();
    assert_eq!(listed, vec![ids[0], ids[2]]);
}

/// A ▶ by id from a page loaded before the song left: refused like a song
/// of another playlist (404), and the engine is told nothing.
#[tokio::test]
async fn a_play_of_a_song_that_left_is_refused() {
    let (state, mut engine_rx) = test_state_with_engine_rx().await;
    let (playlist, ids) = songs(&state.pool, 2).await;
    mark(&state.pool, ids[0]).await;
    let play = |video_id: i64| {
        Request::post(format!("/api/v1/playlists/{playlist}/play-video"))
            .header("content-type", "application/json")
            .body(Body::from(format!(r#"{{"video_id": {video_id}}}"#)))
            .unwrap()
    };
    let router = app(state);
    let resp = router.clone().oneshot(play(ids[0])).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(engine_rx.try_recv().is_err(), "nothing sent to the engine");
    let resp = router.oneshot(play(ids[1])).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT, "a song still in it");
}
