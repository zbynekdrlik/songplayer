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
    fn the_data_dir_is_the_db_dir() {
        let data = std::path::Path::new("data");
        let cfg = ServerConfig {
            db_path: data.join("songplayer.db"),
            ..ServerConfig::default()
        };
        assert_eq!(cfg.data_dir(), data.to_path_buf());
        // A bare DB file name keeps every join relative to the current dir.
        assert_eq!(
            ServerConfig::default().data_dir().join("bench"),
            PathBuf::from("bench")
        );
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
            resolume_registry: Arc::new(resolume::ResolumeRegistry::new()),
            ndi_health_registry: Arc::new(playback::ndi_health::NdiHealthRegistry::new()),
            ndi_burn_registry: Arc::new(playback::ndi_burn::NdiBurnRegistry::new()),
            preview_registry: Arc::new(playback::preview::PreviewRegistry::new()),
            program_bus: Arc::new(playback::program_bus::ProgramBus::new()),
            lan_status: mdns::new_status_handle(),
            metadata_chain: std::sync::Arc::new(crate::metadata::ProviderChain::new(vec![])),
            decode_bench: Arc::new(crate::diag::decode_bench::DecodeBench::new("bench".into())),
        };

        // Verify the router can be built.
        let _router = api::router(state, None);
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
            resolume_registry: Arc::new(resolume::ResolumeRegistry::new()),
            ndi_health_registry: Arc::new(playback::ndi_health::NdiHealthRegistry::new()),
            ndi_burn_registry: Arc::new(playback::ndi_burn::NdiBurnRegistry::new()),
            preview_registry: Arc::new(playback::preview::PreviewRegistry::new()),
            program_bus: Arc::new(playback::program_bus::ProgramBus::new()),
            lan_status: mdns::new_status_handle(),
            metadata_chain: std::sync::Arc::new(crate::metadata::ProviderChain::new(vec![])),
            decode_bench: Arc::new(crate::diag::decode_bench::DecodeBench::new("bench".into())),
        };

        // Verify clone works.
        let _state2 = state.clone();

        // Verify obs_state is readable.
        let obs = state.obs_state.read().await;
        assert!(!obs.connected);
    }

    // -----------------------------------------------------------------
    // Periodic playlist re-sync interval (#139)
    // -----------------------------------------------------------------

    #[test]
    fn sync_interval_from_defaults_when_env_absent() {
        assert_eq!(sync_interval_from(None), 600);
    }

    #[test]
    fn sync_interval_from_parses_valid_override() {
        assert_eq!(sync_interval_from(Some("120")), 120);
    }

    #[test]
    fn sync_interval_from_defaults_on_unparseable_value() {
        assert_eq!(sync_interval_from(Some("abc")), 600);
    }

    #[test]
    fn sync_interval_from_defaults_on_zero() {
        assert_eq!(sync_interval_from(Some("0")), 600);
    }

    // -----------------------------------------------------------------
    // The genlock pacing flag at startup (#147)
    // -----------------------------------------------------------------

    /// A fresh database has no `genlock_pacing` row: pacing is ON, the
    /// owner's rule. SP-program takes only paced sources.
    #[tokio::test]
    async fn genlock_pacing_is_on_for_a_fresh_database() {
        let pool = db::create_memory_pool().await.unwrap();
        db::run_migrations(&pool).await.unwrap();
        assert!(genlock_pacing_setting(&pool).await);
    }

    /// A read that fails (here a closed pool) is not "off" either.
    #[tokio::test]
    async fn genlock_pacing_is_on_when_the_setting_cannot_be_read() {
        let pool = db::create_memory_pool().await.unwrap();
        db::run_migrations(&pool).await.unwrap();
        pool.close().await;
        assert!(genlock_pacing_setting(&pool).await);
    }

    /// Only an explicit "false" selects the SDK-clocked legacy path.
    #[tokio::test]
    async fn genlock_pacing_is_off_only_for_an_explicit_false() {
        let pool = db::create_memory_pool().await.unwrap();
        db::run_migrations(&pool).await.unwrap();
        for (value, want) in [
            ("false", false),
            ("true", true),
            ("garbage", true),
            ("", true),
        ] {
            db::models::set_setting(&pool, "genlock_pacing", value)
                .await
                .unwrap();
            assert_eq!(genlock_pacing_setting(&pool).await, want, "{value:?}");
        }
    }
}
