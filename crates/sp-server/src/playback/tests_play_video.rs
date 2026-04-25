//! Tests for `handle_play_video` — extracted from `tests.rs` via `#[path]`
//! so `tests.rs` stays under the 1000-line airuleset cap.

#![allow(unused_imports)]

use super::*;
use tokio::sync::{broadcast, mpsc};

/// handle_play_video on a custom playlist must update playlists.current_position
/// to match the clicked item's position so the next Skip advances from there.
#[tokio::test]
async fn handle_play_video_updates_current_position_on_custom_playlist() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();

    // Seed videos under a youtube playlist so paths and FKs resolve.
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url) VALUES (50, 'src', 'https://yt.com/src')",
    )
    .execute(&pool)
    .await
    .unwrap();
    for (vid, slug) in [(100_i64, "a"), (200_i64, "b")] {
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, normalized, file_path, audio_file_path) \
             VALUES (?, 50, ?, 1, ?, ?)",
        )
        .bind(vid)
        .bind(slug)
        .bind(format!("/cache/{slug}_video.mp4"))
        .bind(format!("/cache/{slug}_audio.flac"))
        .execute(&pool)
        .await
        .unwrap();
    }

    // Seed the custom playlist items. ytlive is created by ensure_live_playlist_exists.
    crate::startup::ensure_live_playlist_exists(&pool)
        .await
        .unwrap();
    let ytlive_id: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name='ytlive'")
        .fetch_one(&pool)
        .await
        .unwrap();
    crate::db::models::append_playlist_item(&pool, ytlive_id, 100)
        .await
        .unwrap();
    crate::db::models::append_playlist_item(&pool, ytlive_id, 200)
        .await
        .unwrap();

    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (ws_tx, mut ws_rx) = broadcast::channel::<ServerMsg>(16);
    let mut engine = PlaybackEngine::new(
        pool.clone(),
        std::path::PathBuf::from("/tmp/test-cache"),
        obs_tx,
        None,
        resolume_tx,
        ws_tx,
        None,
    );
    engine.ensure_pipeline(ytlive_id, "SP-live");

    // Jump to video 200 (position 1).
    engine.handle_play_video(ytlive_id, 200).await;

    // DB side-effect: current_position advanced to 1.
    let pos: i64 = sqlx::query_scalar("SELECT current_position FROM playlists WHERE id = ?")
        .bind(ytlive_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(pos, 1);

    // Engine side-effect: pipeline bookkeeping updated.
    let pp = engine.pipelines.get(&ytlive_id).unwrap();
    assert_eq!(pp.current_video_id, Some(200));
    assert_eq!(pp.state, PlayState::Playing { video_id: 200 });

    // WS broadcast.
    match ws_rx.try_recv() {
        Ok(ServerMsg::PlaybackStateChanged {
            playlist_id, state, ..
        }) => {
            assert_eq!(playlist_id, ytlive_id);
            assert_eq!(state, WsPlaybackState::Playing);
        }
        other => panic!("expected PlaybackStateChanged(Playing), got {other:?}"),
    }
}

/// handle_play_video with an unknown video_id is a no-op: no DB change, no
/// pipeline update, no WS broadcast.
#[tokio::test]
async fn handle_play_video_with_unknown_video_is_noop() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    crate::startup::ensure_live_playlist_exists(&pool)
        .await
        .unwrap();
    let ytlive_id: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name='ytlive'")
        .fetch_one(&pool)
        .await
        .unwrap();

    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (ws_tx, mut ws_rx) = broadcast::channel::<ServerMsg>(16);
    let mut engine = PlaybackEngine::new(
        pool.clone(),
        std::path::PathBuf::from("/tmp/test-cache"),
        obs_tx,
        None,
        resolume_tx,
        ws_tx,
        None,
    );
    engine.ensure_pipeline(ytlive_id, "SP-live");

    // Video 999 does not exist.
    engine.handle_play_video(ytlive_id, 999).await;

    let pos: i64 = sqlx::query_scalar("SELECT current_position FROM playlists WHERE id = ?")
        .bind(ytlive_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(pos, 0, "unknown video must not touch current_position");

    assert!(
        ws_rx.try_recv().is_err(),
        "no WS broadcast for unknown video"
    );
}
