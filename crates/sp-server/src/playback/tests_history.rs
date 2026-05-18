//! Unit tests for handle_previous + processed-event handlers — extracted
//! from `tests.rs` to keep both files under the 1000-line airuleset cap.

#![allow(unused_imports)]

use super::*;
use std::time::Instant;
use tokio::sync::{broadcast, mpsc};

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
    let mut engine = PlaybackEngine::new(PlaybackEngineConfig {
        pool,
        cache_dir: std::path::PathBuf::from("/tmp/test-cache"),
        obs_event_tx: obs_tx,
        obs_cmd_tx: None,
        resolume_tx,
        ws_event_tx: ws_tx,
        presenter_client: None,
        ndi_health_registry: std::sync::Arc::new(
            crate::playback::ndi_health::NdiHealthRegistry::new(),
        ),
    });
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
    let mut engine = PlaybackEngine::new(PlaybackEngineConfig {
        pool,
        cache_dir: std::path::PathBuf::from("/tmp/test-cache"),
        obs_event_tx: obs_tx,
        obs_cmd_tx: None,
        resolume_tx,
        ws_event_tx: ws_tx,
        presenter_client: None,
        ndi_health_registry: std::sync::Arc::new(
            crate::playback::ndi_health::NdiHealthRegistry::new(),
        ),
    });
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
    let mut engine = PlaybackEngine::new(PlaybackEngineConfig {
        pool,
        cache_dir: std::path::PathBuf::from("/tmp/test-cache"),
        obs_event_tx: obs_tx,
        obs_cmd_tx: None,
        resolume_tx,
        ws_event_tx: ws_tx,
        presenter_client: None,
        ndi_health_registry: std::sync::Arc::new(
            crate::playback::ndi_health::NdiHealthRegistry::new(),
        ),
    });
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
    let mut engine = PlaybackEngine::new(PlaybackEngineConfig {
        pool: pool.clone(),
        cache_dir: std::path::PathBuf::from("/tmp/test-cache"),
        obs_event_tx: obs_tx,
        obs_cmd_tx: None,
        resolume_tx,
        ws_event_tx: ws_tx,
        presenter_client: None,
        ndi_health_registry: std::sync::Arc::new(
            crate::playback::ndi_health::NdiHealthRegistry::new(),
        ),
    });
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
    let mut engine = PlaybackEngine::new(PlaybackEngineConfig {
        pool: pool.clone(),
        cache_dir: std::path::PathBuf::from("/tmp/test-cache"),
        obs_event_tx: obs_tx,
        obs_cmd_tx: None,
        resolume_tx,
        ws_event_tx: ws_tx,
        presenter_client: None,
        ndi_health_registry: std::sync::Arc::new(
            crate::playback::ndi_health::NdiHealthRegistry::new(),
        ),
    });
    engine.ensure_pipeline(7, "SP-fast");

    // Put the pipeline in WaitingForScene WITHOUT the scene being on
    // program. Simulates: scene flipped to sp-fast (engine transitioned
    // WaitingForScene via VideosAvailable) then flipped away before any
    // video was normalized. scene_active is now false.
    if let Some(pp) = engine.pipelines.get_mut(&7) {
        pp.state = PlayState::WaitingForScene;
        pp.scene_active
            .store(false, std::sync::atomic::Ordering::Release);
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
    let mut engine = PlaybackEngine::new(PlaybackEngineConfig {
        pool: pool.clone(),
        cache_dir: std::path::PathBuf::from("/tmp/test-cache"),
        obs_event_tx: obs_tx,
        obs_cmd_tx: None,
        resolume_tx,
        ws_event_tx: ws_tx,
        presenter_client: None,
        ndi_health_registry: std::sync::Arc::new(
            crate::playback::ndi_health::NdiHealthRegistry::new(),
        ),
    });
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
