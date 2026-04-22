//! SongPlayer server — all business logic.

pub mod ai;
pub mod api;
pub mod db;
pub mod downloader;
pub mod lyrics;
pub mod metadata;
pub mod obs;
mod obs_bridge;
pub mod playback;
pub mod playlist;
pub mod presenter;
pub mod reprocess;
pub mod resolume;
pub mod startup;

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
    pub resolume_tx: mpsc::Sender<resolume::ResolumeCommand>,
    /// Signal — sent by playlist CRUD handlers so the OBS client can rebuild
    /// its NDI source map.
    pub obs_rebuild_tx: broadcast::Sender<()>,
    /// Directory where cached media and lyrics JSON files are stored.
    pub cache_dir: PathBuf,
    pub ai_proxy: Arc<ai::proxy::ProxyManager>,
    pub ai_client: Arc<ai::client::AiClient>,
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
    /// Go back to the previous track. Pops the most recent entry off
    /// the per-playlist history stack maintained by `PlaybackEngine`
    /// and plays it. No-op if the history is empty.
    Previous {
        playlist_id: i64,
    },
    SetMode {
        playlist_id: i64,
        mode: PlaybackMode,
    },
    /// Jump to a specific video within a playlist and start playing it
    /// immediately. For custom playlists, also updates
    /// `playlists.current_position` so subsequent Skip advances from the
    /// new position. For youtube playlists it behaves like Previous
    /// (plays the given video but does not affect the random-unplayed
    /// selector; the next Skip will pick a fresh random video).
    PlayVideo {
        playlist_id: i64,
        video_id: i64,
    },
}

/// Status of external tool availability.
#[derive(Debug, Clone, Default)]
pub struct ToolsStatus {
    pub ytdlp_available: bool,
    pub ffmpeg_available: bool,
    pub ytdlp_version: Option<String>,
}

// scene_change_commands and run_obs_engine_bridge live in obs_bridge.rs
use obs_bridge::run_obs_engine_bridge;
#[cfg(test)]
pub(crate) use obs_bridge::scene_change_commands;

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
    startup::ensure_live_playlist_exists(&pool).await?;
    info!("database ready");

    // Self-heal cache: delete legacy single-mp4s, delete orphans,
    // re-link complete pairs. Non-fatal on error.
    if let Err(e) = startup::self_heal_cache(&pool, &config.cache_dir).await {
        tracing::warn!("self-heal cache failed (non-fatal): {e}");
    }

    // 2. Channels
    let (shutdown_tx, _) = broadcast::channel::<()>(1);
    let (event_tx, _) = broadcast::channel::<ServerMsg>(256);
    let (engine_tx, mut engine_rx) = mpsc::channel::<EngineCommand>(64);
    // Rebuild signal from playlist CRUD → OBS client.
    let (obs_rebuild_tx, _) = broadcast::channel::<()>(16);

    // 3. Shared state
    let obs_state = Arc::new(RwLock::new(obs::ObsState::default()));
    let tools_status = Arc::new(RwLock::new(ToolsStatus::default()));
    let tool_paths: Arc<RwLock<Option<ToolPaths>>> = Arc::new(RwLock::new(None));
    let (sync_tx, mut sync_rx) = mpsc::channel::<SyncRequest>(64);
    let (resolume_cmd_tx, mut resolume_cmd_rx) = mpsc::channel::<resolume::ResolumeCommand>(64);

    // Read AI settings from DB or use defaults
    let ai_api_url = db::models::get_setting(&pool, sp_core::config::SETTING_AI_API_URL)
        .await?
        .unwrap_or_else(|| sp_core::config::DEFAULT_AI_API_URL.to_string());
    let ai_model = db::models::get_setting(&pool, sp_core::config::SETTING_AI_MODEL)
        .await?
        .unwrap_or_else(|| sp_core::config::DEFAULT_AI_MODEL.to_string());

    let ai_settings = ai::AiSettings {
        api_url: ai_api_url,
        api_key: None,
        model: ai_model,
        system_prompt_extra: None,
    };
    let ai_client = Arc::new(ai::client::AiClient::new(ai_settings));

    let state = AppState {
        pool: pool.clone(),
        event_tx: event_tx.clone(),
        engine_tx: engine_tx.clone(),
        obs_state: obs_state.clone(),
        tools_status: tools_status.clone(),
        tool_paths: tool_paths.clone(),
        sync_tx: sync_tx.clone(),
        resolume_tx: resolume_cmd_tx.clone(),
        obs_rebuild_tx: obs_rebuild_tx.clone(),
        cache_dir: config.cache_dir.clone(),
        ai_proxy: Arc::new(ai::proxy::ProxyManager::new(
            config.cache_dir.clone(),
            ai::proxy::ProxyManager::default_port(),
        )),
        ai_client: ai_client.clone(),
    };

    // Auto-start the CLIProxyAPI child process + start a watchdog that
    // periodically re-launches it if it dies. Without this, every
    // SongPlayer restart (including CI deploys) leaves the proxy
    // unstarted — the description provider, text-merge, and all other
    // Claude calls silently fall through to "no text sources available"
    // for most songs (2026-04-19 event: 100% of in-flight songs failed
    // gather_sources until POST /api/v1/ai/proxy/start was hit manually).
    if state.ai_proxy.is_claude_authenticated() {
        match state.ai_proxy.start().await {
            Ok(()) => info!("ai_proxy: auto-started CLIProxyAPI at boot"),
            Err(e) => warn!("ai_proxy: auto-start failed (watchdog will retry): {e}"),
        }
        let watchdog_proxy = state.ai_proxy.clone();
        let watchdog_shutdown = shutdown_tx.subscribe();
        tokio::spawn(ai_proxy_watchdog(watchdog_proxy, watchdog_shutdown));
    } else {
        info!("ai_proxy: not authenticated, skipping auto-start + watchdog");
    }

    // 4. Read Gemini settings (used by download worker + reprocess worker)
    let gemini_key = db::models::get_setting(&pool, "gemini_api_key")
        .await?
        .unwrap_or_default();
    let gemini_model = db::models::get_setting(&pool, "gemini_model")
        .await?
        .unwrap_or_else(|| sp_core::config::DEFAULT_GEMINI_MODEL.to_string());

    // Migrate stale gemini_model setting from old defaults.
    let gemini_model = if gemini_model == "gemini-2.0-flash" || gemini_model == "gemini-2.5-flash" {
        let new_model = sp_core::config::DEFAULT_GEMINI_MODEL;
        tracing::info!("upgrading gemini_model setting from {gemini_model} to {new_model}");
        db::models::set_setting(&pool, "gemini_model", new_model).await?;
        new_model.to_string()
    } else {
        gemini_model
    };

    // 5. Tools manager
    let tools_dir = config.cache_dir.join("tools");
    let tools_mgr = downloader::tools::ToolsManager::new(tools_dir.clone());

    // Download worker broadcast channel. Hoisted out of the tools-setup
    // task so the engine can subscribe before tools become ready — that
    // way the engine never misses a `processed:<id>` event, which is
    // how a freshly-normalized video rewakes pipelines parked in
    // WaitingForScene after the 0.11 FLAC migration reset the cache.
    let (dl_event_tx, _dl_event_rx_placeholder) = broadcast::channel::<String>(64);
    let dl_event_tx_for_worker = dl_event_tx.clone();

    let tools_status_clone = tools_status.clone();
    let tools_event_tx = event_tx.clone();
    let tool_paths_clone = tool_paths.clone();
    let dl_pool = pool.clone();
    let dl_cache_dir = config.cache_dir.clone();
    let dl_shutdown_tx = shutdown_tx.clone();
    let dl_gemini_key = gemini_key.clone();
    let dl_gemini_model = gemini_model.clone();
    let startup_sync_pool = pool.clone();
    let startup_sync_tx = sync_tx.clone();
    let lyrics_pool = pool.clone();
    let lyrics_cache_dir = config.cache_dir.clone();
    let lyrics_gemini_key = gemini_key.clone();
    let lyrics_gemini_model = gemini_model.clone();
    let lyrics_shutdown = shutdown_tx.clone();
    let lyrics_tools_dir = tools_dir;
    let ai_client_for_dl = ai_client.clone();
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
                *tool_paths_clone.write().await = Some(paths.clone());
                info!("tools ready: yt-dlp and FFmpeg available");

                // Startup sync fires AFTER tools are ready so the sync
                // worker doesn't silently drop the requests.
                if let Err(e) =
                    startup::startup_sync_active_playlists(&startup_sync_pool, &startup_sync_tx)
                        .await
                {
                    tracing::warn!("startup sync enqueue failed: {e}");
                }

                let mut dl_providers: Vec<Box<dyn metadata::MetadataProvider>> = vec![];
                // Claude first (via CLIProxyAPI), Gemini as fallback
                dl_providers.push(Box::new(metadata::claude::ClaudeMetadataProvider::new(
                    ai_client_for_dl.clone(),
                )));
                if !dl_gemini_key.is_empty() {
                    dl_providers.push(Box::new(metadata::gemini::GeminiProvider::new(
                        dl_gemini_key,
                        dl_gemini_model,
                    )));
                }

                let lyrics_ytdlp = paths.ytdlp.clone();
                let lyrics_python = paths.python.clone();
                let dl_worker = downloader::DownloadWorker::new(
                    dl_pool,
                    paths,
                    dl_cache_dir,
                    dl_providers,
                    dl_event_tx_for_worker,
                );
                tokio::spawn(dl_worker.run(dl_shutdown_tx.subscribe()));
                info!("download worker started");

                // Lyrics worker
                let lyrics_pool_for_loop = lyrics_pool.clone();
                let lyrics_worker = lyrics::LyricsWorker::new(
                    lyrics_pool,
                    lyrics_cache_dir,
                    lyrics_ytdlp,
                    lyrics_python,
                    lyrics_tools_dir,
                    lyrics_gemini_key,
                    lyrics_gemini_model,
                    Some(ai_client_for_dl),
                    tools_event_tx.clone(),
                );
                let current_processing_handle = lyrics_worker.current_processing();
                tokio::spawn(lyrics_worker.run(lyrics_shutdown.subscribe()));
                info!("lyrics worker started");

                // Lyrics queue-update broadcast loop (every 2s → WS clients)
                tokio::spawn(crate::lyrics::worker::queue_update_loop(
                    lyrics_pool_for_loop,
                    tools_event_tx.clone(),
                    current_processing_handle,
                    lyrics_shutdown.subscribe(),
                ));
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

    // 7. OBS WebSocket client + OBS→engine bridge
    //
    // The bridge subscribes to obs_event_tx BEFORE the OBS client spawns.
    // On a fast LAN the OBS client can connect, authenticate, rebuild the
    // NDI source map, and broadcast the initial SceneChanged event in
    // under 50 ms — fast enough to beat a subscription that happens after
    // the spawn. Subscribing first guarantees the bridge never misses the
    // initial scene detection, which is what triggers auto-play on startup.
    let (obs_event_tx, _) = broadcast::channel::<obs::ObsEvent>(64);

    // Bridge: subscribe BEFORE the OBS client spawns so the initial
    // SceneChanged event is never lost to a subscription race.
    {
        let obs_event_rx = obs_event_tx.subscribe();
        let bridge_engine_tx = engine_tx.clone();
        let bridge_shutdown = shutdown_tx.subscribe();
        tokio::spawn(run_obs_engine_bridge(
            obs_event_rx,
            bridge_engine_tx,
            bridge_shutdown,
        ));
    }

    let mut obs_cmd_tx: Option<tokio::sync::mpsc::Sender<obs::ObsCommand>> = None;
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
        let obs_client = obs::ObsClient::spawn(
            obs_config,
            pool.clone(),
            ndi_sources,
            obs_state.clone(),
            obs_event_tx.clone(),
            obs_rebuild_tx.subscribe(),
            shutdown_tx.subscribe(),
        );
        obs_cmd_tx = Some(obs_client.cmd_sender());
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
            .await?;
    let mut resolume_registry = resolume::ResolumeRegistry::new();
    for row in resolume_rows {
        let host_id: i64 = row.get("id");
        let host: String = row.get("host");
        let port: i32 = row.get("port");
        resolume_registry.add_host(host_id, host, port as u16, shutdown_tx.subscribe());
    }
    // Forward commands from the shared channel to all host workers.
    // Uses try_send to avoid blocking the broadcast loop on a slow Resolume host;
    // dropped messages are logged at debug level for observability.
    let resolume_senders = resolume_registry.host_senders();
    tokio::spawn(async move {
        while let Some(cmd) = resolume_cmd_rx.recv().await {
            for tx in &resolume_senders {
                if let Err(e) = tx.try_send(cmd.clone()) {
                    tracing::debug!(%e, "Resolume command dropped (channel full or closed)");
                }
            }
        }
    });

    // 10. Playback engine (bridges API commands to the engine state machine)
    let mut engine = playback::PlaybackEngine::new(
        pool.clone(),
        config.cache_dir.clone(),
        obs_event_tx,
        obs_cmd_tx,
        resolume_cmd_tx,
        event_tx.clone(),
    );

    // Pre-create pipelines for all active playlists so NDI sources appear immediately.
    let active_playlists = db::models::get_active_playlists(&pool)
        .await
        .unwrap_or_default();
    for pl in &active_playlists {
        if !pl.ndi_output_name.is_empty() {
            engine.ensure_pipeline(pl.id, &pl.ndi_output_name);
        }
    }
    info!(
        count = active_playlists.len(),
        "playback pipelines created for active playlists"
    );

    // Engine subscribes to the download worker's broadcast so that
    // `processed:<youtube_id>` events can rewake pipelines stuck in
    // `WaitingForScene`. Subscribing BEFORE spawning the engine loop
    // guarantees no event is missed between tools-ready and first
    // processed video.
    let mut dl_event_rx = dl_event_tx.subscribe();

    let mut engine_shutdown = shutdown_tx.subscribe();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                // Handle API commands (play, pause, skip, etc.)
                Some(cmd) = engine_rx.recv() => {
                    match cmd {
                        EngineCommand::Play { playlist_id } => {
                            // Manual Play from the dashboard: mirror the
                            // scene-active path so the engine state
                            // machine gets the same VideosAvailable +
                            // SceneOn sequence that handle_scene_change
                            // now performs internally.
                            engine.handle_scene_change(playlist_id, true).await;
                        }
                        EngineCommand::Pause { playlist_id } => {
                            engine.handle_command(playlist_id, playback::state::PlayEvent::SceneOff).await;
                        }
                        EngineCommand::Skip { playlist_id } => {
                            engine.handle_command(playlist_id, playback::state::PlayEvent::Skip).await;
                        }
                        EngineCommand::Previous { playlist_id } => {
                            // Pops one entry off the per-playlist history
                            // stack and plays it. See
                            // `PlaybackEngine::handle_previous` for the
                            // full contract.
                            engine.handle_previous(playlist_id).await;
                        }
                        EngineCommand::SetMode { playlist_id, mode } => {
                            engine.handle_command(playlist_id, playback::state::PlayEvent::SetMode(mode)).await;
                        }
                        EngineCommand::PlayVideo { playlist_id, video_id } => {
                            engine.handle_play_video(playlist_id, video_id).await;
                        }
                        EngineCommand::SceneChanged { playlist_id, on_program } => {
                            // VideosAvailable + SceneOn (on program) or
                            // SceneOff (off program) are folded into
                            // handle_scene_change so every caller goes
                            // through the same sequence.
                            engine.handle_scene_change(playlist_id, on_program).await;
                        }
                    }
                }
                // Handle pipeline events (started, position, ended, error)
                Some((playlist_id, event)) = engine.recv_pipeline_event() => {
                    engine.handle_pipeline_event(playlist_id, event).await;
                }
                // Handle download worker broadcasts. The message format
                // is `<kind>:<youtube_id>` where kind is `downloading`
                // or `processed`. Only `processed` rewakes pipelines.
                Ok(msg) = dl_event_rx.recv() => {
                    if let Some(youtube_id) = msg.strip_prefix("processed:") {
                        engine.on_video_processed(youtube_id).await;
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
    let listener = {
        use socket2::{Domain, Socket, Type};
        let socket = Socket::new(Domain::IPV4, Type::STREAM, None)?;
        socket.set_reuse_address(true)?;
        socket.set_nonblocking(true)?;
        socket.bind(&std::net::SocketAddr::from(([0, 0, 0, 0], config.port)).into())?;
        socket.listen(128)?;
        tokio::net::TcpListener::from_std(socket.into())?
    };
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

/// Interval between ai_proxy health checks.
const AI_PROXY_WATCHDOG_INTERVAL_SECS: u64 = 30;

/// Poll the CLIProxyAPI child every `AI_PROXY_WATCHDOG_INTERVAL_SECS` and
/// restart it if it died. Without this, a proxy crash mid-processing
/// leaves the worker silently falling through to "no text sources
/// available" for every subsequent song (2026-04-19 event). Exits on
/// shutdown broadcast.
async fn ai_proxy_watchdog(
    proxy: Arc<ai::proxy::ProxyManager>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    let interval = std::time::Duration::from_secs(AI_PROXY_WATCHDOG_INTERVAL_SECS);
    loop {
        tokio::select! {
            _ = shutdown.recv() => return,
            _ = tokio::time::sleep(interval) => {
                let status = proxy.status().await;
                if status.running {
                    continue;
                }
                warn!("ai_proxy watchdog: proxy is down, attempting restart");
                match proxy.start().await {
                    Ok(()) => info!("ai_proxy watchdog: restart succeeded"),
                    Err(e) => warn!("ai_proxy watchdog: restart failed: {e}"),
                }
            }
        }
    }
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
        };

        // Verify clone works.
        let _state2 = state.clone();

        // Verify obs_state is readable.
        let obs = state.obs_state.read().await;
        assert!(!obs.connected);
    }
}
