//! Integration test for scene-driven playback trigger (issue #11).
//!
//! Exercises the **real** production wire-up from `ObsClient::spawn` through
//! the NDI-source map rebuild and into the `ObsEvent::SceneChanged`
//! broadcast. This is the test that would have caught the `HashMap::new()`
//! bug where the NDI source map was created empty and never populated.
//!
//! Previous unit tests exercised `check_scene_items` against a
//! hand-built HashMap. Those passed while production was broken. This
//! integration test exercises the path that matters.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use common::{FakeObsServer, FakeObsState};
use sp_server::db;
use sp_server::obs;
use tokio::sync::{RwLock, broadcast};

#[tokio::test]
async fn scene_change_to_sp_fast_marks_playlist_7_active() {
    // 1. Seed an in-memory DB with a ytfast playlist whose NDI output name is SP-fast.
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();

    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active)
         VALUES (7, 'ytfast', 'https://youtube.com/playlist?list=PLfast', 'SP-fast', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    // 2. Configure FakeObsServer so it exposes:
    //    - An input `sp-fast_video` of kind `ndi_source`
    //    - That input's settings contain the NDI plugin's full
    //      network-visible name: `"RESOLUME-SNV (SP-fast)"` (machine
    //      hostname + `(stream)` suffix — matching what a real OBS NDI
    //      receiver sees on the wire).
    //    - A scene `sp-fast` containing `sp-fast_video` as an item
    let mut fake_state = FakeObsState::default();
    fake_state
        .inputs
        .insert("sp-fast_video".into(), "ndi_source".into());
    fake_state.input_settings.insert(
        "sp-fast_video".into(),
        serde_json::json!({ "ndi_source_name": "RESOLUME-SNV (SP-fast)" }),
    );
    fake_state.scene_items.insert(
        "sp-fast".into(),
        vec![("sp-fast_video".into(), false, "ndi_source".into())],
    );

    let fake_obs = FakeObsServer::spawn_with_state(fake_state).await;

    // 3. Spawn the real ObsClient pointing at the fake OBS.
    let ndi_sources: obs::NdiSourceMap = Arc::new(RwLock::new(HashMap::new()));
    let obs_state = Arc::new(RwLock::new(obs::ObsState::default()));
    let (obs_event_tx, mut obs_event_rx) = broadcast::channel::<obs::ObsEvent>(16);
    let (_obs_rebuild_tx, obs_rebuild_rx) = broadcast::channel::<()>(4);
    let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(1);

    let _client = obs::ObsClient::spawn(
        obs::ObsConfig {
            url: fake_obs.url(),
            password: None,
        },
        pool.clone(),
        ndi_sources.clone(),
        obs_state.clone(),
        obs_event_tx.clone(),
        obs_rebuild_rx,
        shutdown_rx,
    );

    // 4. Wait for the client to connect, complete the handshake, and run the
    //    initial rebuild. The rebuild hits GetInputList + GetInputSettings
    //    synchronously right after Identified.
    let connect_deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if obs_state.read().await.connected {
            break;
        }
        if std::time::Instant::now() > connect_deadline {
            panic!("ObsClient did not report connected within 5s");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Give the rebuild + initial GetCurrentProgramScene a moment to run.
    tokio::time::sleep(Duration::from_millis(250)).await;

    // 5. Verify the NDI source map was populated from the DB + OBS inputs.
    {
        let map = ndi_sources.read().await;
        assert_eq!(
            map.get("sp-fast_video"),
            Some(&7),
            "ndi_sources should map 'sp-fast_video' → playlist 7, got {map:?}"
        );
    }

    // Drain any scene-change events from the initial GetCurrentProgramScene
    // response (the fake server does not default a current scene, so these
    // are probably just Connected). We want to observe the NEXT scene change.
    while let Ok(evt) = obs_event_rx.try_recv() {
        // Connected / SceneChanged(empty) discarded.
        let _ = evt;
    }

    // 6. Push a CurrentProgramSceneChanged event for `sp-fast` and wait for
    //    the ObsClient to propagate a SceneChanged event upstream.
    fake_obs.push_program_scene_change("sp-fast").await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let active_ids = loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, obs_event_rx.recv()).await {
            Ok(Ok(obs::ObsEvent::SceneChanged {
                scene_name,
                active_playlist_ids,
            })) if scene_name == "sp-fast" => break active_playlist_ids,
            Ok(Ok(_other)) => continue,
            Ok(Err(e)) => panic!("event channel error: {e}"),
            Err(_) => panic!("did not receive SceneChanged for sp-fast within 3s"),
        }
    };

    assert!(
        active_ids.contains(&7),
        "active_playlist_ids should contain 7, got {active_ids:?}"
    );

    // 7. Verify shared ObsState reflects it.
    {
        let s = obs_state.read().await;
        assert_eq!(s.current_scene.as_deref(), Some("sp-fast"));
        assert!(s.active_playlist_ids.contains(&7));
    }

    let _ = shutdown_tx.send(());
    fake_obs.shutdown().await;
}

#[tokio::test]
async fn scene_change_to_scene_without_ndi_source_yields_empty_active() {
    // A scene that does NOT contain any NDI source must produce an empty
    // active set — even after the map is populated. This kills the
    // "always-return-the-same-set" mutation.
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();

    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active)
         VALUES (7, 'ytfast', 'https://yt/f', 'SP-fast', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let mut fake_state = FakeObsState::default();
    fake_state
        .inputs
        .insert("sp-fast_video".into(), "ndi_source".into());
    fake_state.input_settings.insert(
        "sp-fast_video".into(),
        serde_json::json!({ "ndi_source_name": "RESOLUME-SNV (SP-fast)" }),
    );
    // Scene `Break` has only a non-NDI source.
    fake_state.scene_items.insert(
        "Break".into(),
        vec![("Still Image".into(), false, "image_source".into())],
    );

    let fake_obs = FakeObsServer::spawn_with_state(fake_state).await;

    let ndi_sources: obs::NdiSourceMap = Arc::new(RwLock::new(HashMap::new()));
    let obs_state = Arc::new(RwLock::new(obs::ObsState::default()));
    let (obs_event_tx, mut obs_event_rx) = broadcast::channel::<obs::ObsEvent>(16);
    let (_obs_rebuild_tx, obs_rebuild_rx) = broadcast::channel::<()>(4);
    let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(1);

    let _client = obs::ObsClient::spawn(
        obs::ObsConfig {
            url: fake_obs.url(),
            password: None,
        },
        pool.clone(),
        ndi_sources.clone(),
        obs_state.clone(),
        obs_event_tx.clone(),
        obs_rebuild_rx,
        shutdown_rx,
    );

    // Wait for connect.
    let connect_deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if obs_state.read().await.connected {
            break;
        }
        if std::time::Instant::now() > connect_deadline {
            panic!("connect timeout");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(250)).await;

    while let Ok(_evt) = obs_event_rx.try_recv() {}

    fake_obs.push_program_scene_change("Break").await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let active_ids = loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, obs_event_rx.recv()).await {
            Ok(Ok(obs::ObsEvent::SceneChanged {
                scene_name,
                active_playlist_ids,
            })) if scene_name == "Break" => break active_playlist_ids,
            Ok(Ok(_)) => continue,
            Ok(Err(e)) => panic!("event channel error: {e}"),
            Err(_) => panic!("did not receive SceneChanged for Break within 3s"),
        }
    };

    assert!(
        active_ids.is_empty(),
        "Break scene should have no active playlists, got {active_ids:?}"
    );

    let _ = shutdown_tx.send(());
    fake_obs.shutdown().await;
}
