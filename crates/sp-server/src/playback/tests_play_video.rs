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
        std::sync::Arc::new(crate::playback::ndi_health::NdiHealthRegistry::new()),
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

    // WS broadcast. PlayVideo first dispatches `clear_lyrics_display` to
    // avoid the previous song's last line lingering on the wall during the
    // new song's intro — that emits a LyricsUpdate-with-all-None ahead of
    // the PlaybackStateChanged. Drain LyricsUpdate(s) and assert the
    // PlaybackStateChanged eventually arrives.
    let mut found_play = false;
    while let Ok(msg) = ws_rx.try_recv() {
        if let ServerMsg::PlaybackStateChanged {
            playlist_id, state, ..
        } = msg
        {
            assert_eq!(playlist_id, ytlive_id);
            assert_eq!(state, WsPlaybackState::Playing);
            found_play = true;
            break;
        }
    }
    assert!(
        found_play,
        "expected PlaybackStateChanged(Playing) in WS stream"
    );
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
        std::sync::Arc::new(crate::playback::ndi_health::NdiHealthRegistry::new()),
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

/// Regression: Started event MUST unconditionally set lyrics_state (to Some or
/// None) — it must never leave a stale value from a prior song. Covers the
/// Ok(None) branch: has_lyrics=0 → load returns None → lyrics_state=None.
#[tokio::test]
async fn started_event_unconditionally_resets_lyrics_state() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();

    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (7, 'p', 'u', 'SP-fast', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, song, artist, has_lyrics, normalized) \
         VALUES (1, 7, 'no_lyrics_id', 'Song', 'Artist', 0, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (ws_tx, _) = broadcast::channel::<ServerMsg>(16);
    let mut engine = PlaybackEngine::new(
        pool,
        std::path::PathBuf::from("/tmp/test-cache"),
        obs_tx,
        None,
        resolume_tx,
        ws_tx,
        None,
        std::sync::Arc::new(crate::playback::ndi_health::NdiHealthRegistry::new()),
    );
    engine.ensure_pipeline(7, "SP-fast");
    if let Some(pp) = engine.pipelines.get_mut(&7) {
        pp.current_video_id = Some(1);
    }

    engine
        .handle_pipeline_event(
            7,
            PipelineEvent::Started {
                duration_ms: 60_000,
            },
        )
        .await;

    let pp = engine.pipelines.get(&7).unwrap();
    assert!(
        pp.lyrics_state.is_none(),
        "Started with has_lyrics=0 must set lyrics_state=None, not preserve stale state"
    );
}

/// Regression: Started event with malformed lyrics JSON on disk MUST set
/// lyrics_state=None and warn — not panic, not preserve stale state.
/// Covers the Err(e) branch of load_lyrics_for_video.
#[tokio::test]
async fn started_event_with_malformed_lyrics_warns_and_clears_state() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();

    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (7, 'p', 'u', 'SP-fast', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    // has_lyrics=1 forces the loader to attempt the disk read; the malformed
    // JSON below makes serde_json::from_str fail → Err branch.
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, song, artist, has_lyrics, normalized) \
         VALUES (1, 7, 'malformed_id', 'Song', 'Artist', 1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let cache_dir = tempfile::tempdir().unwrap();
    tokio::fs::write(
        cache_dir.path().join("malformed_id_lyrics.json"),
        b"{ this is not valid JSON }",
    )
    .await
    .unwrap();

    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (ws_tx, _) = broadcast::channel::<ServerMsg>(16);
    let mut engine = PlaybackEngine::new(
        pool,
        cache_dir.path().to_path_buf(),
        obs_tx,
        None,
        resolume_tx,
        ws_tx,
        None,
        std::sync::Arc::new(crate::playback::ndi_health::NdiHealthRegistry::new()),
    );
    engine.ensure_pipeline(7, "SP-fast");
    if let Some(pp) = engine.pipelines.get_mut(&7) {
        pp.current_video_id = Some(1);
    }

    engine
        .handle_pipeline_event(
            7,
            PipelineEvent::Started {
                duration_ms: 60_000,
            },
        )
        .await;

    let pp = engine.pipelines.get(&7).unwrap();
    assert!(
        pp.lyrics_state.is_none(),
        "Started with malformed lyrics JSON must set lyrics_state=None, not panic or preserve stale state"
    );
}
