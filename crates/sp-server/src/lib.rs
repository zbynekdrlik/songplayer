//! SongPlayer server — all business logic.

pub mod api;
pub mod db;
pub mod downloader;
pub mod metadata;
pub mod obs;
pub mod playback;
pub mod playlist;
pub mod reprocess;
pub mod resolume;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use sp_core::playback::PlaybackMode;
use sp_core::ws::ServerMsg;
use sqlx::{Row, SqlitePool};
use tokio::sync::{RwLock, broadcast, mpsc};
use tracing::{info, warn};

use crate::downloader::tools::ToolPaths;

// ---------------------------------------------------------------------------
// Shared application state
// ---------------------------------------------------------------------------

/// A request to sync a playlist with its YouTube source.
#[derive(Debug, Clone)]
pub struct SyncRequest {
    pub playlist_id: i64,
    pub youtube_url: String,
}

/// Shared state passed to all Axum handlers and background workers.
#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub event_tx: broadcast::Sender<ServerMsg>,
    pub engine_tx: mpsc::Sender<EngineCommand>,
    pub obs_state: Arc<RwLock<obs::ObsState>>,
    pub tools_status: Arc<RwLock<ToolsStatus>>,
    pub tool_paths: Arc<RwLock<Option<ToolPaths>>>,
    pub sync_tx: mpsc::Sender<SyncRequest>,
}

/// Commands sent from the API layer to the playback engine.
#[derive(Debug, Clone)]
pub enum EngineCommand {
    SceneChanged {
        playlist_id: i64,
        on_program: bool,
    },
    Play {
        playlist_id: i64,
    },
    Pause {
        playlist_id: i64,
    },
    Skip {
        playlist_id: i64,
    },
    SetMode {
        playlist_id: i64,
        mode: PlaybackMode,
    },
}

/// Status of external tool availability.
#[derive(Debug, Clone, Default)]
pub struct ToolsStatus {
    pub ytdlp_available: bool,
    pub ffmpeg_available: bool,
    pub ytdlp_version: Option<String>,
}

// ---------------------------------------------------------------------------
// Server configuration
// ---------------------------------------------------------------------------

/// Configuration for the server startup.
pub struct ServerConfig {
    pub db_path: PathBuf,
    pub cache_dir: PathBuf,
    pub port: u16,
    /// Directory containing the WASM frontend (`dist/`). If set, serves static files.
    pub dist_dir: Option<PathBuf>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            db_path: PathBuf::from("songplayer.db"),
            cache_dir: PathBuf::from("cache"),
            port: sp_core::config::DEFAULT_API_PORT,
            dist_dir: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Server entry point
// ---------------------------------------------------------------------------

/// Start the server. Blocks until shutdown signal.
///
/// Orchestrates all subsystems:
/// 1. SQLite pool + migrations
/// 2. Broadcast channels for events
/// 3. Shared state (incl. tool_paths + sync channel)
/// 4. Gemini settings (for download + reprocess workers)
/// 5. Tools manager (yt-dlp + FFmpeg) + download worker
/// 6. Sync handler (playlist sync worker)
/// 7. OBS WebSocket client
/// 8. Reprocess worker (with Gemini provider)
/// 9. Resolume workers
/// 10. Playback engine
/// 11. Axum HTTP server
/// 12. Shutdown signal
pub async fn start(
    config: ServerConfig,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> Result<(), anyhow::Error> {
    // 1. Database
    let pool = db::create_pool(&format!("sqlite:{}", config.db_path.display())).await?;
    db::run_migrations(&pool).await?;
    info!("database ready");

    // 2. Channels
    let (shutdown_tx, _) = broadcast::channel::<()>(1);
    let (event_tx, _) = broadcast::channel::<ServerMsg>(256);
    let (engine_tx, mut engine_rx) = mpsc::channel::<EngineCommand>(64);

    // 3. Shared state
    let obs_state = Arc::new(RwLock::new(obs::ObsState::default()));
    let tools_status = Arc::new(RwLock::new(ToolsStatus::default()));
    let tool_paths: Arc<RwLock<Option<ToolPaths>>> = Arc::new(RwLock::new(None));
    let (sync_tx, mut sync_rx) = mpsc::channel::<SyncRequest>(64);

    let state = AppState {
        pool: pool.clone(),
        event_tx: event_tx.clone(),
        engine_tx: engine_tx.clone(),
        obs_state: obs_state.clone(),
        tools_status: tools_status.clone(),
        tool_paths: tool_paths.clone(),
        sync_tx: sync_tx.clone(),
    };

    // 4. Read Gemini settings (used by download worker + reprocess worker)
    let gemini_key = db::models::get_setting(&pool, "gemini_api_key")
        .await?
        .unwrap_or_default();
    let gemini_model = db::models::get_setting(&pool, "gemini_model")
        .await?
        .unwrap_or_else(|| "gemini-2.0-flash".to_string());

    // 5. Tools manager
    let tools_dir = config.cache_dir.join("tools");
    let tools_mgr = downloader::tools::ToolsManager::new(tools_dir);

    let tools_status_clone = tools_status.clone();
    let tools_event_tx = event_tx.clone();
    let tool_paths_clone = tool_paths.clone();
    let dl_pool = pool.clone();
    let dl_cache_dir = config.cache_dir.clone();
    let dl_shutdown_tx = shutdown_tx.clone();
    let dl_gemini_key = gemini_key.clone();
    let dl_gemini_model = gemini_model.clone();
    tokio::spawn(async move {
        match tools_mgr.ensure_tools().await {
            Ok(paths) => {
                let version = tools_mgr.ytdlp_version(&paths.ytdlp).await.ok();
                let mut ts = tools_status_clone.write().await;
                ts.ytdlp_available = true;
                ts.ffmpeg_available = true;
                ts.ytdlp_version = version.clone();
                let _ = tools_event_tx.send(ServerMsg::ToolsStatus {
                    ytdlp_available: true,
                    ffmpeg_available: true,
                    ytdlp_version: version,
                });
                // Store resolved tool paths for sync workers.
                *tool_paths_clone.write().await = Some(paths.clone());
                info!("tools ready: yt-dlp and FFmpeg available");

                // Build metadata providers for the download worker.
                let mut dl_providers: Vec<Box<dyn metadata::MetadataProvider>> = vec![];
                if !dl_gemini_key.is_empty() {
                    dl_providers.push(Box::new(metadata::gemini::GeminiProvider::new(
                        dl_gemini_key,
                        dl_gemini_model,
                    )));
                }

                // Spawn download worker now that tools are available.
                let (dl_event_tx, _) = broadcast::channel::<String>(64);
                let dl_worker = downloader::DownloadWorker::new(
                    dl_pool,
                    paths,
                    dl_cache_dir,
                    dl_providers,
                    dl_event_tx,
                );
                tokio::spawn(dl_worker.run(dl_shutdown_tx.subscribe()));
                info!("download worker started");
            }
            Err(e) => {
                tracing::error!("tools setup failed: {e}");
            }
        }
    });

    // 6. Sync handler — receives SyncRequests and calls playlist::sync_playlist
    let sync_pool = pool.clone();
    let sync_tool_paths = tool_paths.clone();
    tokio::spawn(async move {
        while let Some(req) = sync_rx.recv().await {
            let paths = sync_tool_paths.read().await;
            let Some(ref tp) = *paths else {
                warn!(
                    playlist_id = req.playlist_id,
                    "sync request received but tools not yet available, dropping"
                );
                continue;
            };
            let ytdlp = tp.ytdlp.clone();
            drop(paths); // release read lock before awaiting sync

            match playlist::sync_playlist(&sync_pool, req.playlist_id, &req.youtube_url, &ytdlp)
                .await
            {
                Ok(new_count) => {
                    info!(
                        playlist_id = req.playlist_id,
                        new_count, "playlist sync complete"
                    );
                }
                Err(e) => {
                    warn!(playlist_id = req.playlist_id, "playlist sync failed: {e}");
                }
            }
        }
    });

    // 7. OBS WebSocket client
    let (obs_event_tx, _) = broadcast::channel::<obs::ObsEvent>(64);
    let obs_url = db::models::get_setting(&pool, "obs_websocket_url")
        .await?
        .unwrap_or_default();
    if !obs_url.is_empty() {
        let obs_password = db::models::get_setting(&pool, "obs_password")
            .await?
            .unwrap_or_default();
        let obs_config = obs::ObsConfig {
            url: obs_url,
            password: if obs_password.is_empty() {
                None
            } else {
                Some(obs_password)
            },
        };
        let ndi_sources: obs::NdiSourceMap = Arc::new(RwLock::new(HashMap::new()));
        let _obs_client = obs::ObsClient::spawn(
            obs_config,
            ndi_sources,
            obs_state.clone(),
            obs_event_tx.clone(),
            shutdown_tx.subscribe(),
        );
        info!("OBS WebSocket client started");
    }

    // 8. Reprocess worker (with Gemini provider if API key is configured)
    let mut reprocess_provider_list: Vec<Box<dyn metadata::MetadataProvider>> = vec![];
    if !gemini_key.is_empty() {
        reprocess_provider_list.push(Box::new(metadata::gemini::GeminiProvider::new(
            gemini_key,
            gemini_model,
        )));
    }
    let reprocess_providers: Arc<Vec<Box<dyn metadata::MetadataProvider>>> =
        Arc::new(reprocess_provider_list);
    let reprocess_worker = reprocess::ReprocessWorker::new(
        pool.clone(),
        reprocess_providers,
        config.cache_dir.clone(),
    );
    tokio::spawn(reprocess_worker.run(shutdown_tx.subscribe()));

    // 9. Resolume workers (load enabled hosts from DB)
    let resolume_rows =
        sqlx::query("SELECT id, host, port FROM resolume_hosts WHERE is_enabled = 1")
            .fetch_all(&pool)
            .await
            .unwrap_or_default();
    let mut _resolume_registry = resolume::ResolumeRegistry::new();
    for row in resolume_rows {
        let host_id: i64 = row.get("id");
        let host: String = row.get("host");
        let port: i32 = row.get("port");
        _resolume_registry.add_host(host_id, host, port as u16, shutdown_tx.subscribe());
    }

    // 10. Playback engine (bridges API commands to the engine state machine)
    let mut engine = playback::PlaybackEngine::new(pool.clone(), obs_event_tx);
    let mut engine_shutdown = shutdown_tx.subscribe();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(cmd) = engine_rx.recv() => {
                    match cmd {
                        EngineCommand::Play { playlist_id } => {
                            engine.handle_command(playlist_id, playback::state::PlayEvent::SceneOn).await;
                        }
                        EngineCommand::Pause { playlist_id } => {
                            engine.handle_command(playlist_id, playback::state::PlayEvent::SceneOff).await;
                        }
                        EngineCommand::Skip { playlist_id } => {
                            engine.handle_command(playlist_id, playback::state::PlayEvent::Skip).await;
                        }
                        EngineCommand::SetMode { playlist_id, mode } => {
                            engine.handle_command(playlist_id, playback::state::PlayEvent::SetMode(mode)).await;
                        }
                        EngineCommand::SceneChanged { playlist_id, on_program } => {
                            engine.handle_scene_change(playlist_id, on_program).await;
                        }
                    }
                }
                _ = engine_shutdown.recv() => {
                    info!("engine command bridge shutting down");
                    break;
                }
            }
        }
    });

    // 11. Axum HTTP server
    let router = api::router(state, config.dist_dir);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", config.port)).await?;
    info!(port = config.port, "HTTP server listening");

    // Serve with graceful shutdown
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.recv().await;
            info!("shutdown signal received, stopping HTTP server");
        })
        .await?;

    // 12. Signal all workers to stop
    let _ = shutdown_tx.send(());
    info!("server stopped");

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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

        let state = AppState {
            pool,
            event_tx,
            engine_tx,
            obs_state,
            tools_status,
            tool_paths: Arc::new(RwLock::new(None)),
            sync_tx,
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

        let state = AppState {
            pool,
            event_tx,
            engine_tx,
            obs_state: Arc::new(RwLock::new(obs::ObsState::default())),
            tools_status: Arc::new(RwLock::new(ToolsStatus::default())),
            tool_paths: Arc::new(RwLock::new(None)),
            sync_tx,
        };

        // Verify clone works.
        let _state2 = state.clone();

        // Verify obs_state is readable.
        let obs = state.obs_state.read().await;
        assert!(!obs.connected);
    }
}
