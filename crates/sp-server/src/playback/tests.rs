//! Unit tests for the playback engine — extracted from `mod.rs` via
//! `#[path]` so the engine file stays under 1000 lines.

#![allow(unused_imports)]

use super::*;
use std::time::Instant;
use tokio::sync::{broadcast, mpsc};

#[test]
fn engine_construction() {
    // Verify the engine can be constructed without panicking.
    // We use a fake pool — the engine doesn't touch the DB at construction.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        let (obs_tx, _obs_rx) = broadcast::channel(16);
        let (resolume_tx, _) = mpsc::channel(16);
        let (ws_tx, _) = broadcast::channel::<ServerMsg>(16);
        let engine = PlaybackEngine::new(pool, obs_tx, None, resolume_tx, ws_tx);
        assert!(engine.pipelines.is_empty());
    });
}

#[test]
fn engine_ensure_pipeline_creates_entry() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        let (obs_tx, _obs_rx) = broadcast::channel(16);
        let (resolume_tx, _) = mpsc::channel(16);
        let (ws_tx, _) = broadcast::channel::<ServerMsg>(16);
        let mut engine = PlaybackEngine::new(pool, obs_tx, None, resolume_tx, ws_tx);

        engine.ensure_pipeline(1, "TestNDI");
        assert!(engine.pipelines.contains_key(&1));

        // Calling again should not create a second pipeline.
        engine.ensure_pipeline(1, "TestNDI");
        assert_eq!(engine.pipelines.len(), 1);
    });
}

#[test]
fn engine_ensure_pipeline_multiple_playlists() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        let (obs_tx, _obs_rx) = broadcast::channel(16);
        let (resolume_tx, _) = mpsc::channel(16);
        let (ws_tx, _) = broadcast::channel::<ServerMsg>(16);
        let mut engine = PlaybackEngine::new(pool, obs_tx, None, resolume_tx, ws_tx);

        engine.ensure_pipeline(1, "NDI-1");
        engine.ensure_pipeline(2, "NDI-2");
        assert_eq!(engine.pipelines.len(), 2);
    });
}

/// Regression test for stale title-timer bug: when a video is skipped,
/// the previous video's title-show/hide timers must be cancelled so they
/// don't fire mid-song during the next video.
#[test]
fn cancel_title_timers_aborts_pending_handles() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        // Spawn a long-running task and grab its abort handle.
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _ = started_tx.send(());
            // This sleep represents the 1.5s/N seconds title timer.
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            "should not reach here"
        });
        // Wait for the task to actually start.
        started_rx.await.unwrap();

        let mut pp = PlaylistPipeline {
            pipeline: PlaybackPipeline::spawn(
                "test".to_string(),
                None,
                mpsc::unbounded_channel().0,
                1,
            ),
            state: PlayState::Idle,
            mode: PlaybackMode::default(),
            current_video_id: None,
            title_show_abort: Some(task.abort_handle()),
            title_hide_abort: None,
            cached_song: String::new(),
            cached_artist: String::new(),
            cached_duration_ms: 0,
            last_now_playing_broadcast: None,
        };

        assert!(pp.title_show_abort.is_some());
        pp.cancel_title_timers();
        assert!(pp.title_show_abort.is_none());

        // Verify the underlying task was actually aborted.
        let result = task.await;
        assert!(
            result.is_err(),
            "task should have been aborted, got: {result:?}"
        );
        assert!(result.unwrap_err().is_cancelled());
    });
}

/// Verify get_video_title_info returns the actual song+artist from the DB.
/// Kills mutants that replace the function body with constants.
#[tokio::test]
async fn get_video_title_info_returns_song_and_artist() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();

    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'P', 'url')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO videos (id, playlist_id, youtube_id, song, artist) VALUES (42, 1, 'abc', 'My Song', 'Artist Name')")
        .execute(&pool)
        .await
        .unwrap();

    let result = get_video_title_info(&pool, 42).await.unwrap();
    assert_eq!(
        result,
        Some(("My Song".to_string(), "Artist Name".to_string()))
    );
}

#[tokio::test]
async fn get_video_title_info_returns_none_for_missing_video() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let result = get_video_title_info(&pool, 999).await.unwrap();
    assert_eq!(result, None);
}

#[tokio::test]
async fn get_video_title_info_handles_null_song_and_artist() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'P', 'url')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO videos (id, playlist_id, youtube_id) VALUES (42, 1, 'abc')")
        .execute(&pool)
        .await
        .unwrap();
    let result = get_video_title_info(&pool, 42).await.unwrap();
    assert_eq!(result, Some((String::new(), String::new())));
}

/// Regression for issue #9: a pipeline `Started` event must produce a
/// `ServerMsg::NowPlaying` broadcast with song/artist/duration pulled
/// from the DB. Before the fix, the engine had no `ws_event_tx` and
/// nothing ever reached the dashboard.
#[tokio::test]
async fn pipeline_started_event_broadcasts_now_playing() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();

    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name) \
         VALUES (1, 'P', 'url', 'SP-p')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, song, artist) \
         VALUES (42, 1, 'abc123', 'Test Song', 'Test Artist')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (ws_tx, mut ws_rx) = broadcast::channel::<ServerMsg>(16);
    let mut engine = PlaybackEngine::new(pool, obs_tx, None, resolume_tx, ws_tx);
    engine.ensure_pipeline(1, "SP-p");

    // Simulate a video having been selected (so current_video_id is set).
    if let Some(pp) = engine.pipelines.get_mut(&1) {
        pp.current_video_id = Some(42);
    }

    engine
        .handle_pipeline_event(
            1,
            PipelineEvent::Started {
                duration_ms: 180_000,
            },
        )
        .await;

    // The first message on ws_rx should be our NowPlaying.
    // (PlaybackStateChanged may follow, but NowPlaying must be present.)
    let mut saw_now_playing = false;
    for _ in 0..4 {
        match tokio::time::timeout(std::time::Duration::from_millis(500), ws_rx.recv()).await {
            Ok(Ok(ServerMsg::NowPlaying {
                playlist_id,
                video_id,
                song,
                artist,
                position_ms,
                duration_ms,
            })) => {
                assert_eq!(playlist_id, 1);
                assert_eq!(video_id, 42);
                assert_eq!(song, "Test Song");
                assert_eq!(artist, "Test Artist");
                assert_eq!(position_ms, 0);
                assert_eq!(duration_ms, 180_000);
                saw_now_playing = true;
                break;
            }
            Ok(Ok(_other)) => continue,
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }
    assert!(saw_now_playing, "expected a NowPlaying broadcast");
}

/// Direct test of `play_state_to_ws` across every variant — kills the
/// `Default::default()` mutant which returns the wrong variant on
/// non-Idle inputs.
#[test]
fn play_state_to_ws_maps_all_variants() {
    assert_eq!(play_state_to_ws(&PlayState::Idle), WsPlaybackState::Idle);
    assert_eq!(
        play_state_to_ws(&PlayState::WaitingForScene),
        WsPlaybackState::WaitingForScene
    );
    assert_eq!(
        play_state_to_ws(&PlayState::Playing { video_id: 42 }),
        WsPlaybackState::Playing
    );
}

/// Boundary test for the pure throttle predicate `should_send_position_update`.
/// Kills the `>=` → `<`, `==`, `>` mutants at exact boundary values —
/// something the parent method cannot test reliably under coverage
/// tooling because `Instant::now()` races against the test setup.
#[test]
fn should_send_position_update_boundary_checks() {
    // Throttle window is 500 ms.
    // 0 ms elapsed → within window → no send.
    assert!(!should_send_position_update(0));
    // 499 ms elapsed → within window → no send. Kills the `<` mutant.
    assert!(!should_send_position_update(499));
    // 500 ms elapsed → boundary → send (because `>=`). Kills the `>`
    // mutant (which would require strict greater-than).
    assert!(should_send_position_update(500));
    // 501 ms elapsed → beyond window → send. Kills the `==` mutant.
    assert!(should_send_position_update(501));
    // Large values always send.
    assert!(should_send_position_update(u64::MAX));
}

/// Direct test of `apply_event` — kills the `-> ()` mutant (whole
/// function replaced with no-op) and the `!=` → `==` mutant (which
/// would flip the broadcast guard so transitions silently stop firing).
///
/// Drives the engine through a real state transition and asserts both
/// that `pp.state` changed AND that a `PlaybackStateChanged` broadcast
/// was emitted on the ws channel.
#[tokio::test]
async fn apply_event_triggers_state_change_and_broadcast() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();

    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'P', 'u')")
        .execute(&pool)
        .await
        .unwrap();

    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (ws_tx, mut ws_rx) = broadcast::channel::<ServerMsg>(64);
    let mut engine = PlaybackEngine::new(pool, obs_tx, None, resolume_tx, ws_tx);
    engine.ensure_pipeline(1, "TestNDI");

    // Idle + VideosAvailable → WaitingForScene (state change).
    engine.apply_event(1, PlayEvent::VideosAvailable).await;

    assert_eq!(
        engine.pipelines.get(&1).unwrap().state,
        PlayState::WaitingForScene,
        "apply_event should have advanced the state"
    );

    // Expect exactly one PlaybackStateChanged on the ws channel with
    // state=WaitingForScene.
    match ws_rx.try_recv() {
        Ok(ServerMsg::PlaybackStateChanged {
            playlist_id, state, ..
        }) => {
            assert_eq!(playlist_id, 1);
            assert_eq!(state, WsPlaybackState::WaitingForScene);
        }
        other => panic!("expected PlaybackStateChanged(WaitingForScene), got {other:?}"),
    }
}

/// Kills the `!=` → `==` mutant in `apply_event`'s broadcast guard.
/// When an event does NOT change state (e.g. VideosAvailable applied
/// twice in a row), no second broadcast should be produced.
#[tokio::test]
async fn apply_event_no_broadcast_when_state_unchanged() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();

    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'P', 'u')")
        .execute(&pool)
        .await
        .unwrap();

    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (ws_tx, mut ws_rx) = broadcast::channel::<ServerMsg>(64);
    let mut engine = PlaybackEngine::new(pool, obs_tx, None, resolume_tx, ws_tx);
    engine.ensure_pipeline(1, "TestNDI");

    // First transition: Idle → WaitingForScene — broadcast expected.
    engine.apply_event(1, PlayEvent::VideosAvailable).await;
    let _first = ws_rx.try_recv().expect("first transition broadcasts");

    // Second VideosAvailable: state stays WaitingForScene — NO broadcast.
    engine.apply_event(1, PlayEvent::VideosAvailable).await;
    assert!(
        ws_rx.try_recv().is_err(),
        "state unchanged: no PlaybackStateChanged should be broadcast"
    );
}

/// Fast-firing `Position` events must not flood the broadcast channel:
/// only one `NowPlaying` should be sent per `POSITION_BROADCAST_INTERVAL_MS`.
///
/// Uses real time (not `tokio::time::pause`) because the sqlite pool
/// setup before the throttle check relies on real I/O which blocks
/// indefinitely under a paused timer.
#[tokio::test]
async fn position_events_are_throttled() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();

    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name) \
         VALUES (1, 'P', 'url', 'SP-p')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, song, artist) \
         VALUES (42, 1, 'abc123', 'Song', 'Artist')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (ws_tx, mut ws_rx) = broadcast::channel::<ServerMsg>(64);
    let mut engine = PlaybackEngine::new(pool, obs_tx, None, resolume_tx, ws_tx);
    engine.ensure_pipeline(1, "SP-p");
    if let Some(pp) = engine.pipelines.get_mut(&1) {
        pp.current_video_id = Some(42);
    }

    engine
        .handle_pipeline_event(
            1,
            PipelineEvent::Started {
                duration_ms: 180_000,
            },
        )
        .await;

    // Drain messages produced by Started (NowPlaying + possibly PlaybackStateChanged).
    while ws_rx.try_recv().is_ok() {}

    // Fire 10 Position events in quick succession.
    for i in 1..=10u64 {
        engine
            .handle_pipeline_event(
                1,
                PipelineEvent::Position {
                    position_ms: i * 10,
                    duration_ms: 180_000,
                },
            )
            .await;
    }

    // Within the 500ms throttle window (10 rapid events fired in a few
    // microseconds), the only broadcast that should have been produced
    // is the initial one on Started (already drained). Zero additional
    // NowPlaying should be visible yet.
    assert!(
        ws_rx.try_recv().is_err(),
        "no NowPlaying should leak while within the 500ms throttle window"
    );

    // Sleep past the throttle window and fire once more — should
    // produce exactly one additional broadcast.
    tokio::time::sleep(std::time::Duration::from_millis(550)).await;
    engine
        .handle_pipeline_event(
            1,
            PipelineEvent::Position {
                position_ms: 700,
                duration_ms: 180_000,
            },
        )
        .await;

    match ws_rx.try_recv() {
        Ok(ServerMsg::NowPlaying { position_ms, .. }) => {
            assert_eq!(position_ms, 700);
        }
        other => panic!("expected NowPlaying after throttle window, got {other:?}"),
    }
}
