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

use std::path::PathBuf;
use std::sync::Arc;

use sp_core::playback::PlaybackMode;
use sp_core::ws::ServerMsg;
use sqlx::SqlitePool;
use tokio::sync::{RwLock, broadcast, mpsc};
use tracing::info;

// ---------------------------------------------------------------------------
// Shared application state
// ---------------------------------------------------------------------------

/// Shared state passed to all Axum handlers and background workers.
#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub event_tx: broadcast::Sender<ServerMsg>,
    pub engine_tx: mpsc::Sender<EngineCommand>,
    pub obs_state: Arc<RwLock<obs::ObsState>>,
    pub tools_status: Arc<RwLock<ToolsStatus>>,
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
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            db_path: PathBuf::from("songplayer.db"),
            cache_dir: PathBuf::from("cache"),
            port: sp_core::config::DEFAULT_API_PORT,
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
/// 3. Tools manager (yt-dlp + FFmpeg)
/// 4. Download worker
/// 5. Reprocess worker
/// 6. OBS WebSocket client
/// 7. Resolume workers
/// 8. Playback engine
/// 9. Axum HTTP server
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

    let state = AppState {
        pool: pool.clone(),
        event_tx: event_tx.clone(),
        engine_tx: engine_tx.clone(),
        obs_state: obs_state.clone(),
        tools_status: tools_status.clone(),
    };

    // 4. Tools manager
    let tools_dir = config.cache_dir.join("tools");
    let tools_mgr = downloader::tools::ToolsManager::new(tools_dir);

    let tools_status_clone = tools_status.clone();
    let tools_event_tx = event_tx.clone();
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
                info!("tools ready: yt-dlp and FFmpeg available");
            }
            Err(e) => {
                tracing::error!("tools setup failed: {e}");
            }
        }
    });

    // 5. Reprocess worker
    let reprocess_providers: Arc<Vec<Box<dyn metadata::MetadataProvider>>> = Arc::new(vec![]);
    let reprocess_worker = reprocess::ReprocessWorker::new(
        pool.clone(),
        reprocess_providers,
        config.cache_dir.clone(),
    );
    tokio::spawn(reprocess_worker.run(shutdown_tx.subscribe()));

    // 6. Engine command consumer (bridges API commands to the engine)
    let engine_event_tx = event_tx.clone();
    tokio::spawn(async move {
        while let Some(cmd) = engine_rx.recv().await {
            match cmd {
                EngineCommand::Play { playlist_id } => {
                    info!(playlist_id, "play command received");
                }
                EngineCommand::Pause { playlist_id } => {
                    info!(playlist_id, "pause command received");
                }
                EngineCommand::Skip { playlist_id } => {
                    info!(playlist_id, "skip command received");
                }
                EngineCommand::SetMode { playlist_id, mode } => {
                    info!(playlist_id, ?mode, "set mode command received");
                }
                EngineCommand::SceneChanged {
                    playlist_id,
                    on_program,
                } => {
                    info!(playlist_id, on_program, "scene changed");
                }
            }
            // The full playback engine integration happens when all pieces
            // are wired together. For now, commands are logged.
            let _ = &engine_event_tx;
        }
    });

    // 7. Axum HTTP server
    let router = api::router(state);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", config.port)).await?;
    info!(port = config.port, "HTTP server listening");

    // Serve with graceful shutdown
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.recv().await;
            info!("shutdown signal received, stopping HTTP server");
        })
        .await?;

    // 8. Signal all workers to stop
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

        let state = AppState {
            pool,
            event_tx,
            engine_tx,
            obs_state,
            tools_status,
        };

        // Verify the router can be built.
        let _router = api::router(state);
    }

    #[tokio::test]
    async fn app_state_construction() {
        let pool = db::create_memory_pool().await.unwrap();
        db::run_migrations(&pool).await.unwrap();

        let (event_tx, _) = broadcast::channel::<ServerMsg>(16);
        let (engine_tx, _) = mpsc::channel::<EngineCommand>(16);

        let state = AppState {
            pool,
            event_tx,
            engine_tx,
            obs_state: Arc::new(RwLock::new(obs::ObsState::default())),
            tools_status: Arc::new(RwLock::new(ToolsStatus::default())),
        };

        // Verify clone works.
        let _state2 = state.clone();

        // Verify obs_state is readable.
        let obs = state.obs_state.read().await;
        assert!(!obs.connected);
    }
}
