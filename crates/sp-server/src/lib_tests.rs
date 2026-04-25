// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn server_config_default() {
        let cfg = ServerConfig::default();
        assert_eq!(cfg.port, sp_core::config::DEFAULT_API_PORT);
        assert_eq!(cfg.db_path, PathBuf::from("songplayer.db"));
        assert_eq!(cfg.cache_dir, PathBuf::from("cache"));
    }

    #[test]
    fn tools_status_default() {
        let ts = ToolsStatus::default();
        assert!(!ts.ytdlp_available);
        assert!(!ts.ffmpeg_available);
        assert!(ts.ytdlp_version.is_none());
    }

    #[test]
    fn engine_command_debug() {
        let cmd = EngineCommand::Play { playlist_id: 42 };
        let dbg = format!("{cmd:?}");
        assert!(dbg.contains("Play"));
        assert!(dbg.contains("42"));
    }

    #[test]
    fn app_state_is_clone() {
        // Verify AppState can be cloned (required by Axum).
        fn assert_clone<T: Clone>() {}
        assert_clone::<AppState>();
    }

    #[tokio::test]
    async fn start_and_shutdown() {
        // Use in-memory DB to avoid file system.
        let pool = db::create_memory_pool().await.unwrap();
        db::run_migrations(&pool).await.unwrap();

        let (event_tx, _) = broadcast::channel::<ServerMsg>(16);
        let (engine_tx, _) = mpsc::channel::<EngineCommand>(16);
        let obs_state = Arc::new(RwLock::new(obs::ObsState::default()));
        let tools_status = Arc::new(RwLock::new(ToolsStatus::default()));

        let (sync_tx, _) = mpsc::channel::<SyncRequest>(16);
        let (resolume_tx, _) = mpsc::channel::<resolume::ResolumeCommand>(16);

        let (obs_rebuild_tx, _) = broadcast::channel::<()>(4);
        let state = AppState {
            pool,
            event_tx,
            engine_tx,
            obs_state,
            tools_status,
            tool_paths: Arc::new(RwLock::new(None)),
            sync_tx,
            resolume_tx,
            obs_rebuild_tx,
            cache_dir: PathBuf::from("cache"),
            ai_proxy: Arc::new(ai::proxy::ProxyManager::new(
                PathBuf::from("cache"),
                ai::proxy::ProxyManager::default_port(),
            )),
            ai_client: Arc::new(ai::client::AiClient::new(ai::AiSettings::default())),
            presenter_client: None,
        };

        // Verify the router can be built.
        let _router = api::router(state, None);
    }

    // ---------------------------------------------------------------------
    // scene_change_commands + run_obs_engine_bridge tests
    // ---------------------------------------------------------------------

    #[test]
    fn scene_change_commands_empty_to_empty_produces_nothing() {
        let prev = std::collections::HashSet::new();
        let curr = std::collections::HashSet::new();
        assert!(scene_change_commands(&prev, &curr).is_empty());
    }

    #[test]
    fn scene_change_commands_same_set_re_emits_all_on_program_true() {
        // Same set on both sides: we still re-emit `on` for all current
        // playlists. This is the critical behaviour that makes scene
        // re-activation work after an out-of-band state mutation.
        let prev: std::collections::HashSet<i64> = [1, 2, 3].into_iter().collect();
        let curr: std::collections::HashSet<i64> = [1, 2, 3].into_iter().collect();
        let cmds = scene_change_commands(&prev, &curr);
        assert_eq!(cmds, vec![(1, true), (2, true), (3, true)]);
    }

    #[test]
    fn scene_change_commands_add_produces_on_program_true() {
        let prev = std::collections::HashSet::new();
        let curr: std::collections::HashSet<i64> = [7].into_iter().collect();
        let cmds = scene_change_commands(&prev, &curr);
        assert_eq!(cmds, vec![(7, true)]);
    }

    #[test]
    fn scene_change_commands_remove_produces_on_program_false() {
        let prev: std::collections::HashSet<i64> = [7].into_iter().collect();
        let curr = std::collections::HashSet::new();
        let cmds = scene_change_commands(&prev, &curr);
        assert_eq!(cmds, vec![(7, false)]);
    }

    #[test]
    fn scene_change_commands_swap_emits_off_then_on_for_different_ids() {
        // Previous had {2}, current has {7} — expect 2 off, then 7 on.
        let prev: std::collections::HashSet<i64> = [2].into_iter().collect();
        let curr: std::collections::HashSet<i64> = [7].into_iter().collect();
        let cmds = scene_change_commands(&prev, &curr);
        // Offs come before ons so the state machine transitions to
        // WaitingForScene before the new scene kicks in.
        assert_eq!(cmds, vec![(2, false), (7, true)]);
    }

    #[test]
    fn scene_change_commands_partial_overlap() {
        // Previous {1, 2, 3}, current {2, 3, 4}:
        //   - Off: 1
        //   - On: 2, 3, 4  (all currently-active get re-emitted)
        let prev: std::collections::HashSet<i64> = [1, 2, 3].into_iter().collect();
        let curr: std::collections::HashSet<i64> = [2, 3, 4].into_iter().collect();
        let cmds = scene_change_commands(&prev, &curr);
        assert_eq!(cmds, vec![(1, false), (2, true), (3, true), (4, true)]);
    }

    #[tokio::test]
    async fn obs_engine_bridge_forwards_scene_changed_as_engine_commands() {
        use std::collections::HashSet;

        let (obs_event_tx, obs_event_rx) = broadcast::channel::<obs::ObsEvent>(16);
        let (engine_tx, mut engine_rx) = mpsc::channel::<EngineCommand>(16);
        let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(1);

        tokio::spawn(run_obs_engine_bridge(obs_event_rx, engine_tx, shutdown_rx));

        // First event: playlist 7 becomes active.
        let mut active: HashSet<i64> = HashSet::new();
        active.insert(7);
        obs_event_tx
            .send(obs::ObsEvent::SceneChanged {
                scene_name: "sp-fast".into(),
                active_playlist_ids: active,
            })
            .unwrap();

        // Expect a SceneChanged{7, true} on engine_rx.
        let cmd = tokio::time::timeout(std::time::Duration::from_millis(500), engine_rx.recv())
            .await
            .expect("engine command within 500ms")
            .expect("engine channel still open");
        match cmd {
            EngineCommand::SceneChanged {
                playlist_id: 7,
                on_program: true,
            } => {}
            other => panic!("expected SceneChanged{{7, true}}, got {other:?}"),
        }

        // Next event: playlist 7 goes off program (scene switch to empty).
        obs_event_tx
            .send(obs::ObsEvent::SceneChanged {
                scene_name: "Break".into(),
                active_playlist_ids: HashSet::new(),
            })
            .unwrap();

        let cmd = tokio::time::timeout(std::time::Duration::from_millis(500), engine_rx.recv())
            .await
            .expect("engine command within 500ms")
            .expect("engine channel still open");
        match cmd {
            EngineCommand::SceneChanged {
                playlist_id: 7,
                on_program: false,
            } => {}
            other => panic!("expected SceneChanged{{7, false}}, got {other:?}"),
        }

        let _ = shutdown_tx.send(());
    }

    /// Regression: when the engine state is mutated out-of-band (e.g.
    /// via a REST `/pause`) and then the SAME scene event fires again,
    /// the bridge must still re-emit `SceneChanged { on_program: true }`
    /// so the state machine re-enters `SelectAndPlay`.
    ///
    /// Before the fix, `run_obs_engine_bridge` diffed against its own
    /// tracked `previous` set and suppressed identical-scene events,
    /// leaving the engine stuck in `WaitingForScene` indefinitely.
    /// This was caught by the failing combined post-deploy test which
    /// paused ytfast via REST after scene detection had already fired.
    #[tokio::test]
    async fn bridge_re_emits_scene_on_after_external_state_change() {
        use std::collections::HashSet;

        let (obs_event_tx, obs_event_rx) = broadcast::channel::<obs::ObsEvent>(16);
        let (engine_tx, mut engine_rx) = mpsc::channel::<EngineCommand>(16);
        let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(1);

        tokio::spawn(run_obs_engine_bridge(obs_event_rx, engine_tx, shutdown_rx));

        // First scene event: playlist 7 becomes active.
        let active: HashSet<i64> = [7].into_iter().collect();
        obs_event_tx
            .send(obs::ObsEvent::SceneChanged {
                scene_name: "sp-fast".into(),
                active_playlist_ids: active.clone(),
            })
            .unwrap();

        // Drain the first SceneChanged(7, true).
        let cmd = tokio::time::timeout(std::time::Duration::from_millis(500), engine_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            cmd,
            EngineCommand::SceneChanged {
                playlist_id: 7,
                on_program: true
            }
        ));

        // --- Out-of-band state mutation (simulated REST /pause): nothing
        // flows through the bridge, the engine state machine transitions
        // to WaitingForScene on its own. The bridge's internal `previous`
        // set is unchanged and still equals {7}.

        // Second scene event: SAME active set. A naive diff bridge would
        // send nothing. The fixed bridge MUST re-emit SceneChanged(7, true).
        obs_event_tx
            .send(obs::ObsEvent::SceneChanged {
                scene_name: "sp-fast".into(),
                active_playlist_ids: active,
            })
            .unwrap();

        let cmd = tokio::time::timeout(std::time::Duration::from_millis(500), engine_rx.recv())
            .await
            .expect("bridge must re-emit on an identical-scene event")
            .unwrap();
        assert!(
            matches!(
                cmd,
                EngineCommand::SceneChanged {
                    playlist_id: 7,
                    on_program: true
                }
            ),
            "expected re-emitted SceneChanged{{7, true}}, got {cmd:?}"
        );

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn obs_engine_bridge_disconnect_marks_all_previously_active_off() {
        use std::collections::HashSet;

        let (obs_event_tx, obs_event_rx) = broadcast::channel::<obs::ObsEvent>(16);
        let (engine_tx, mut engine_rx) = mpsc::channel::<EngineCommand>(16);
        let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(1);

        tokio::spawn(run_obs_engine_bridge(obs_event_rx, engine_tx, shutdown_rx));

        // First: activate playlists 2 and 7.
        let mut active: HashSet<i64> = HashSet::new();
        active.insert(2);
        active.insert(7);
        obs_event_tx
            .send(obs::ObsEvent::SceneChanged {
                scene_name: "multi".into(),
                active_playlist_ids: active,
            })
            .unwrap();

        // Drain the two "on" commands.
        for _ in 0..2 {
            let _ = tokio::time::timeout(std::time::Duration::from_millis(200), engine_rx.recv())
                .await
                .unwrap();
        }

        // Now disconnect.
        obs_event_tx.send(obs::ObsEvent::Disconnected).unwrap();

        // Expect two "off" commands for playlists 2 and 7 (order not important).
        let mut off_ids: Vec<i64> = Vec::new();
        for _ in 0..2 {
            let cmd = tokio::time::timeout(std::time::Duration::from_millis(500), engine_rx.recv())
                .await
                .expect("off command within 500ms")
                .expect("engine channel still open");
            match cmd {
                EngineCommand::SceneChanged {
                    playlist_id,
                    on_program: false,
                } => off_ids.push(playlist_id),
                other => panic!("expected SceneChanged off, got {other:?}"),
            }
        }
        off_ids.sort();
        assert_eq!(off_ids, vec![2, 7]);

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn app_state_construction() {
        let pool = db::create_memory_pool().await.unwrap();
        db::run_migrations(&pool).await.unwrap();

        let (event_tx, _) = broadcast::channel::<ServerMsg>(16);
        let (engine_tx, _) = mpsc::channel::<EngineCommand>(16);

        let (sync_tx, _) = mpsc::channel::<SyncRequest>(16);
        let (resolume_tx, _) = mpsc::channel::<resolume::ResolumeCommand>(16);
        let (obs_rebuild_tx, _) = broadcast::channel::<()>(4);

        let state = AppState {
            pool,
            event_tx,
            engine_tx,
            obs_state: Arc::new(RwLock::new(obs::ObsState::default())),
            tools_status: Arc::new(RwLock::new(ToolsStatus::default())),
            tool_paths: Arc::new(RwLock::new(None)),
            sync_tx,
            resolume_tx,
            obs_rebuild_tx,
            cache_dir: PathBuf::from("cache"),
            ai_proxy: Arc::new(ai::proxy::ProxyManager::new(
                PathBuf::from("cache"),
                ai::proxy::ProxyManager::default_port(),
            )),
            ai_client: Arc::new(ai::client::AiClient::new(ai::AiSettings::default())),
            presenter_client: None,
        };

        // Verify clone works.
        let _state2 = state.clone();

        // Verify obs_state is readable.
        let obs = state.obs_state.read().await;
        assert!(!obs.connected);
    }
}
