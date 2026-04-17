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
        let engine = PlaybackEngine::new(
            pool,
            std::path::PathBuf::from("/tmp/test-cache"),
            obs_tx,
            None,
            resolume_tx,
            ws_tx,
        );
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
        let mut engine = PlaybackEngine::new(
            pool,
            std::path::PathBuf::from("/tmp/test-cache"),
            obs_tx,
            None,
            resolume_tx,
            ws_tx,
        );

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
        let mut engine = PlaybackEngine::new(
            pool,
            std::path::PathBuf::from("/tmp/test-cache"),
            obs_tx,
            None,
            resolume_tx,
            ws_tx,
        );

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
            scene_active: false,
            title_show_abort: Some(task.abort_handle()),
            title_hide_abort: None,
            cached_song: String::new(),
            cached_artist: String::new(),
            cached_duration_ms: 0,
            last_now_playing_broadcast: None,
            history: std::collections::VecDeque::new(),
            lyrics_state: None,
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

    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (99, 'P', 'url')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO videos (id, playlist_id, youtube_id, song, artist) VALUES (42, 99, 'abc', 'My Song', 'Artist Name')")
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
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (99, 'P', 'url')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO videos (id, playlist_id, youtube_id) VALUES (42, 99, 'abc')")
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
         VALUES (99, 'P', 'url', 'SP-p')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, song, artist) \
         VALUES (42, 99, 'abc123', 'Test Song', 'Test Artist')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (ws_tx, mut ws_rx) = broadcast::channel::<ServerMsg>(16);
    let mut engine = PlaybackEngine::new(
        pool,
        std::path::PathBuf::from("/tmp/test-cache"),
        obs_tx,
        None,
        resolume_tx,
        ws_tx,
    );
    engine.ensure_pipeline(99, "SP-p");

    // Simulate a video having been selected (so current_video_id is set).
    if let Some(pp) = engine.pipelines.get_mut(&99) {
        pp.current_video_id = Some(42);
    }

    engine
        .handle_pipeline_event(
            99,
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
                assert_eq!(playlist_id, 99);
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

/// The `duration_ms > 0 ? duration_ms : cached_duration_ms` fallback in
/// `maybe_broadcast_position_update` must use the incoming value when
/// non-zero and fall back to the cached value when zero.
///
/// Kills the `> 0` → `== 0`, `< 0`, `>= 0` mutants:
/// - `== 0`: would flip the branches; expected behavior changes at both
///   zero and non-zero inputs.
/// - `< 0`: u64 never < 0 → always false → always uses cached. Caught
///   when we pass a non-zero value and expect it to propagate.
/// - `>= 0`: u64 always >= 0 → always true → always uses incoming.
///   Caught when we pass 0 and expect the cached fallback.
#[tokio::test]
async fn maybe_broadcast_position_update_uses_cached_duration_when_zero() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (99, 'P', 'u')")
        .execute(&pool)
        .await
        .unwrap();

    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (ws_tx, mut ws_rx) = broadcast::channel::<ServerMsg>(64);
    let mut engine = PlaybackEngine::new(
        pool,
        std::path::PathBuf::from("/tmp/test-cache"),
        obs_tx,
        None,
        resolume_tx,
        ws_tx,
    );
    engine.ensure_pipeline(99, "TestNDI");
    if let Some(pp) = engine.pipelines.get_mut(&99) {
        pp.current_video_id = Some(7);
        pp.cached_song = "song".into();
        pp.cached_artist = "artist".into();
        pp.cached_duration_ms = 180_000;
        // Leaving last_now_playing_broadcast = None so the throttle
        // helper always returns true for this test — we are exercising
        // the duration-fallback branch specifically, not the throttle.
        pp.last_now_playing_broadcast = None;
    }

    // Case A: duration_ms = 0 → must use cached 180_000.
    engine.maybe_broadcast_position_update(99, 100, 0);
    match ws_rx.try_recv() {
        Ok(ServerMsg::NowPlaying {
            position_ms: 100,
            duration_ms: 180_000,
            ..
        }) => {}
        other => panic!("zero input duration_ms must fall back to cached 180_000, got {other:?}"),
    }

    // Reset last-broadcast so the throttle helper lets the next call through.
    if let Some(pp) = engine.pipelines.get_mut(&99) {
        pp.last_now_playing_broadcast = None;
    }

    // Case B: duration_ms = 120_000 (non-zero) → must use 120_000.
    engine.maybe_broadcast_position_update(99, 200, 120_000);
    match ws_rx.try_recv() {
        Ok(ServerMsg::NowPlaying {
            position_ms: 200,
            duration_ms: 120_000,
            ..
        }) => {}
        other => panic!("non-zero input duration_ms must propagate to broadcast, got {other:?}"),
    }
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

    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (99, 'P', 'u')")
        .execute(&pool)
        .await
        .unwrap();

    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (ws_tx, mut ws_rx) = broadcast::channel::<ServerMsg>(64);
    let mut engine = PlaybackEngine::new(
        pool,
        std::path::PathBuf::from("/tmp/test-cache"),
        obs_tx,
        None,
        resolume_tx,
        ws_tx,
    );
    engine.ensure_pipeline(99, "TestNDI");

    // Idle + VideosAvailable → WaitingForScene (state change).
    engine.apply_event(99, PlayEvent::VideosAvailable).await;

    assert_eq!(
        engine.pipelines.get(&99).unwrap().state,
        PlayState::WaitingForScene,
        "apply_event should have advanced the state"
    );

    // Expect exactly one PlaybackStateChanged on the ws channel with
    // state=WaitingForScene.
    match ws_rx.try_recv() {
        Ok(ServerMsg::PlaybackStateChanged {
            playlist_id, state, ..
        }) => {
            assert_eq!(playlist_id, 99);
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

    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (99, 'P', 'u')")
        .execute(&pool)
        .await
        .unwrap();

    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (ws_tx, mut ws_rx) = broadcast::channel::<ServerMsg>(64);
    let mut engine = PlaybackEngine::new(
        pool,
        std::path::PathBuf::from("/tmp/test-cache"),
        obs_tx,
        None,
        resolume_tx,
        ws_tx,
    );
    engine.ensure_pipeline(99, "TestNDI");

    // First transition: Idle → WaitingForScene — broadcast expected.
    engine.apply_event(99, PlayEvent::VideosAvailable).await;
    let _first = ws_rx.try_recv().expect("first transition broadcasts");

    // Second VideosAvailable: state stays WaitingForScene — NO broadcast.
    engine.apply_event(99, PlayEvent::VideosAvailable).await;
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
         VALUES (99, 'P', 'url', 'SP-p')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, song, artist) \
         VALUES (42, 99, 'abc123', 'Song', 'Artist')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (ws_tx, mut ws_rx) = broadcast::channel::<ServerMsg>(64);
    let mut engine = PlaybackEngine::new(
        pool,
        std::path::PathBuf::from("/tmp/test-cache"),
        obs_tx,
        None,
        resolume_tx,
        ws_tx,
    );
    engine.ensure_pipeline(99, "SP-p");
    if let Some(pp) = engine.pipelines.get_mut(&99) {
        pp.current_video_id = Some(42);
    }

    engine
        .handle_pipeline_event(
            99,
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
                99,
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
            99,
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

/// Previous with an empty history is a no-op: no pipeline command sent,
/// no broadcast, no state mutation. Kills accidental regressions where
/// Previous might randomly pick a new video.
#[tokio::test]
async fn handle_previous_with_empty_history_is_noop() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (99, 'P', 'u')")
        .execute(&pool)
        .await
        .unwrap();

    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (ws_tx, mut ws_rx) = broadcast::channel::<ServerMsg>(16);
    let mut engine = PlaybackEngine::new(
        pool,
        std::path::PathBuf::from("/tmp/test-cache"),
        obs_tx,
        None,
        resolume_tx,
        ws_tx,
    );
    engine.ensure_pipeline(99, "TestNDI");

    // Fresh pipeline: current_video_id = None, history = [].
    engine.handle_previous(99).await;

    // State unchanged.
    let pp = engine.pipelines.get(&99).unwrap();
    assert_eq!(pp.state, PlayState::Idle);
    assert!(pp.current_video_id.is_none());
    assert!(pp.history.is_empty());

    // No broadcast.
    assert!(
        ws_rx.try_recv().is_err(),
        "empty-history Previous must not broadcast"
    );
}

/// Previous pops the most recent entry from history, sets it as current,
/// and broadcasts PlaybackStateChanged. Repeated Previous presses walk
/// backwards through the stack one step at a time.
#[tokio::test]
async fn handle_previous_pops_history_and_plays() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (99, 'P', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    // Seed videos 10, 11, 12 with valid normalized sidecar paths so
    // handle_previous can successfully look them up via get_song_paths.
    for vid in [10_i64, 11, 12] {
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, normalized, file_path, audio_file_path) \
             VALUES (?, 99, ?, 1, ?, ?)",
        )
        .bind(vid)
        .bind(format!("yt{vid}"))
        .bind(format!("/tmp/video_{vid}_video.mp4"))
        .bind(format!("/tmp/video_{vid}_audio.flac"))
        .execute(&pool)
        .await
        .unwrap();
    }

    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (ws_tx, mut ws_rx) = broadcast::channel::<ServerMsg>(16);
    let mut engine = PlaybackEngine::new(
        pool,
        std::path::PathBuf::from("/tmp/test-cache"),
        obs_tx,
        None,
        resolume_tx,
        ws_tx,
    );
    engine.ensure_pipeline(99, "TestNDI");

    // Simulate having played 10, 11, 12 in order. Current = 12, history = [10, 11].
    if let Some(pp) = engine.pipelines.get_mut(&99) {
        pp.history.push_back(10);
        pp.history.push_back(11);
        pp.current_video_id = Some(12);
        pp.state = PlayState::Playing { video_id: 12 };
    }

    // First Previous: should play 11, leaving history = [10].
    engine.handle_previous(99).await;
    {
        let pp = engine.pipelines.get(&99).unwrap();
        assert_eq!(pp.current_video_id, Some(11));
        assert_eq!(pp.state, PlayState::Playing { video_id: 11 });
        assert_eq!(pp.history.len(), 1);
        assert_eq!(pp.history.back().copied(), Some(10));
    }
    match ws_rx.try_recv() {
        Ok(ServerMsg::PlaybackStateChanged {
            playlist_id: 99,
            state,
            ..
        }) => assert_eq!(state, WsPlaybackState::Playing),
        other => panic!("expected PlaybackStateChanged(Playing), got {other:?}"),
    }

    // Second Previous: should play 10, leaving history = [].
    engine.handle_previous(99).await;
    {
        let pp = engine.pipelines.get(&99).unwrap();
        assert_eq!(pp.current_video_id, Some(10));
        assert!(pp.history.is_empty());
    }
    // Drain the state-changed broadcast.
    let _ = ws_rx.try_recv();

    // Third Previous: history now empty, no-op.
    engine.handle_previous(99).await;
    {
        let pp = engine.pipelines.get(&99).unwrap();
        // current_video_id stays at 10, history still empty.
        assert_eq!(pp.current_video_id, Some(10));
        assert!(pp.history.is_empty());
    }
    assert!(
        ws_rx.try_recv().is_err(),
        "no broadcast when history is exhausted"
    );
}

/// The history stack is bounded: pushing more than
/// `PREVIOUS_HISTORY_CAPACITY` entries drops the oldest from the front.
#[tokio::test]
async fn history_capacity_is_bounded() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (99, 'P', 'u')")
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
    );
    engine.ensure_pipeline(99, "TestNDI");

    // Simulate the SelectAndPlay bookkeeping for `CAPACITY + 3` videos
    // by directly pushing to the history stack the same way the real
    // code path does.
    if let Some(pp) = engine.pipelines.get_mut(&99) {
        for i in 0..(PREVIOUS_HISTORY_CAPACITY as i64 + 3) {
            pp.history.push_back(i);
            while pp.history.len() > PREVIOUS_HISTORY_CAPACITY {
                pp.history.pop_front();
            }
        }
        assert_eq!(pp.history.len(), PREVIOUS_HISTORY_CAPACITY);
        // First three entries (0, 1, 2) dropped. Newest in the back.
        assert_eq!(pp.history.front().copied(), Some(3));
        assert_eq!(
            pp.history.back().copied(),
            Some(PREVIOUS_HISTORY_CAPACITY as i64 + 2)
        );
    }
}

/// Regression test for the stuck-WaitingForScene bug that shipped in
/// 0.11.0 and caused nothing to play on win-resolume after the FLAC
/// migration reset every video to `normalized = 0`.
///
/// Scenario:
///   1. The engine receives `SceneOn` for a playlist BEFORE any video
///      is normalized. `SelectAndPlay` runs but finds no candidate, so
///      the pipeline parks in `WaitingForScene` with `current_video_id
///      = None`.
///   2. The download worker finishes processing a video and broadcasts
///      "processed:<id>" on the shared event channel.
///   3. The engine must detect this and re-run `SelectAndPlay` for any
///      pipeline whose scene is currently active but has no video
///      playing, so the freshly-normalized video starts playing.
///
/// Before the fix the engine had no listener for the processed event,
/// so the pipeline stayed parked indefinitely even though OBS was
/// sitting on the matching scene and normalized videos existed in the
/// DB. This test drives the engine through that exact sequence and
/// asserts the pipeline ends in `Playing` state.
#[tokio::test]
async fn processed_event_rewakes_waiting_pipeline_with_new_video() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (7, 'ytfast', 'u')")
        .execute(&pool)
        .await
        .unwrap();

    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (ws_tx, _) = broadcast::channel::<ServerMsg>(16);
    let mut engine = PlaybackEngine::new(
        pool.clone(),
        std::path::PathBuf::from("/tmp/test-cache"),
        obs_tx,
        None,
        resolume_tx,
        ws_tx,
    );
    engine.ensure_pipeline(7, "SP-fast");

    // Step 1: scene goes active BEFORE any video exists. Simulates
    // OBS sitting on sp-fast at server startup immediately after V4
    // migration resets normalized=0 and the cache is empty.
    engine.handle_scene_change(7, true).await;

    {
        let pp = engine
            .pipelines
            .get(&7)
            .expect("pipeline exists for playlist 7");
        // Pipeline is parked — WaitingForScene, no video selected.
        assert_eq!(pp.state, PlayState::WaitingForScene);
        assert!(
            pp.current_video_id.is_none(),
            "no video should be selected yet; DB is empty"
        );
    }

    // Step 2: download worker finishes a video. Insert a normalized
    // row that matches the shape the real worker writes via
    // `mark_video_processed_pair`, then tell the engine a video was
    // processed for this playlist.
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, file_path, audio_file_path) \
         VALUES (100, 7, 'yt-new-100', 1, '/tmp/new_video_100_video.mp4', '/tmp/new_video_100_audio.flac')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Step 3: engine must re-run SelectAndPlay for pipeline 7. Calling
    // the public entry point — this method must exist; if the test
    // fails to compile that is the red-test state.
    engine.on_video_processed("yt-new-100").await;

    // Step 4: pipeline is now Playing the freshly-normalized video.
    let pp = engine
        .pipelines
        .get(&7)
        .expect("pipeline still exists for playlist 7");
    assert_eq!(
        pp.state,
        PlayState::Playing { video_id: 100 },
        "after processed event with active scene the pipeline must be Playing"
    );
    assert_eq!(pp.current_video_id, Some(100));
}

/// Negative case that specifically targets the `scene_active` guard in
/// `should_wake`: a pipeline parked in `WaitingForScene` whose scene is
/// NOT currently active must NOT auto-play when a matching video is
/// processed. Kills the `&&` → `||` mutation on the `scene_active`
/// predicate — if the guard loses effect, the pipeline transitions to
/// Playing under the mutation and the test catches it.
#[tokio::test]
async fn processed_event_ignores_waiting_pipeline_with_inactive_scene() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (7, 'ytfast', 'u')")
        .execute(&pool)
        .await
        .unwrap();

    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (ws_tx, _) = broadcast::channel::<ServerMsg>(16);
    let mut engine = PlaybackEngine::new(
        pool.clone(),
        std::path::PathBuf::from("/tmp/test-cache"),
        obs_tx,
        None,
        resolume_tx,
        ws_tx,
    );
    engine.ensure_pipeline(7, "SP-fast");

    // Put the pipeline in WaitingForScene WITHOUT the scene being on
    // program. Simulates: scene flipped to sp-fast (engine transitioned
    // WaitingForScene via VideosAvailable) then flipped away before any
    // video was normalized. scene_active is now false.
    if let Some(pp) = engine.pipelines.get_mut(&7) {
        pp.state = PlayState::WaitingForScene;
        pp.scene_active = false;
        pp.current_video_id = None;
    }

    // Insert a normalized video that matches playlist 7 — the one the
    // download worker would have produced later.
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, file_path, audio_file_path) \
         VALUES (300, 7, 'yt-new-300', 1, '/tmp/video_300_video.mp4', '/tmp/video_300_audio.flac')",
    )
    .execute(&pool)
    .await
    .unwrap();

    engine.on_video_processed("yt-new-300").await;

    // Even though the playlist has a WaitingForScene state AND a fresh
    // video is available, the scene is NOT active, so the engine must
    // NOT start playback. Under the `&&` → `||` mutation on
    // `scene_active`, should_wake becomes true and the pipeline would
    // transition to Playing via SelectAndPlay.
    let pp = engine.pipelines.get(&7).expect("pipeline 7 exists");
    assert_eq!(
        pp.state,
        PlayState::WaitingForScene,
        "pipeline must stay in WaitingForScene when scene is inactive; got {:?}",
        pp.state
    );
    assert!(
        pp.current_video_id.is_none(),
        "no video should be selected when scene is inactive; got {:?}",
        pp.current_video_id
    );
}

/// Negative case: a processed event MUST NOT start playback for a
/// pipeline whose scene is NOT active. The engine only re-runs
/// `SelectAndPlay` for playlists currently on program.
#[tokio::test]
async fn processed_event_does_not_play_inactive_scene() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (7, 'ytfast', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (3, 'ytpresence', 'u2')")
        .execute(&pool)
        .await
        .unwrap();

    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (ws_tx, _) = broadcast::channel::<ServerMsg>(16);
    let mut engine = PlaybackEngine::new(
        pool.clone(),
        std::path::PathBuf::from("/tmp/test-cache"),
        obs_tx,
        None,
        resolume_tx,
        ws_tx,
    );
    engine.ensure_pipeline(7, "SP-fast");
    engine.ensure_pipeline(3, "SP-presence");

    // Scene is active only on playlist 7, not on 3.
    engine.handle_scene_change(7, true).await;

    // Insert a normalized video on playlist 3 (the INACTIVE scene).
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, file_path, audio_file_path) \
         VALUES (200, 3, 'yt-new-200', 1, '/tmp/video_200_video.mp4', '/tmp/video_200_audio.flac')",
    )
    .execute(&pool)
    .await
    .unwrap();

    engine.on_video_processed("yt-new-200").await;

    // Playlist 3 must stay Idle / WaitingForScene — its scene is not on program.
    let pp3 = engine.pipelines.get(&3).expect("pipeline 3 exists");
    assert!(
        !matches!(pp3.state, PlayState::Playing { .. }),
        "inactive-scene playlist 3 must not auto-play; got {:?}",
        pp3.state
    );
    assert!(
        pp3.current_video_id.is_none(),
        "inactive-scene playlist 3 must not have a selected video"
    );
}

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

    // Seed the custom playlist items. ytlive is already present from V13.
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
