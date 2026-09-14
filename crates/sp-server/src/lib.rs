//! SongPlayer server — all business logic.

pub mod ai;
pub mod api;
pub mod db;
pub mod downloader;
mod engine_command;
pub use engine_command::EngineCommand;
pub mod lyrics;
pub mod mdns;
pub mod metadata;
pub mod obs;
mod obs_bridge;
pub mod panic_hook;
pub mod playback;
pub mod playlist;
pub mod presenter;
pub mod reprocess;
pub mod resolume;
pub mod shutdown;
pub mod startup;
pub mod stems;

pub use panic_hook::install_panic_hook;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

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
    /// Presenter HTTP client; None = push disabled. See `presenter` module.
    pub presenter_client: Option<Arc<presenter::PresenterClient>>,
    /// Resolume registry exposing per-host health snapshots.
    pub resolume_registry: Arc<resolume::ResolumeRegistry>,
    /// NDI health registry exposing per-pipeline health snapshots.
    pub ndi_health_registry: Arc<playback::ndi_health::NdiHealthRegistry>,
    /// Runtime burn-id overlay toggle registry (#151). `POST /api/v1/ndi/burn`
    /// reads/writes it synchronously; the playback engine + pipeline threads
    /// share the same registry (default OFF, never persisted).
    pub ndi_burn_registry: Arc<playback::ndi_burn::NdiBurnRegistry>,
    /// LAN `sp.local` advertisement status (#51) — written by the mDNS task,
    /// read by `/api/v1/status` so the dashboard shows the offline-LAN URL.
    pub lan_status: mdns::LanStatusHandle,
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
    // Install the panic hook FIRST so any panic during startup or steady-state
    // is captured to a durable crash file before release `panic = "abort"`
    // kills the process (#156). Idempotent: the Tauri shell installs it earlier
    // when present, and the internal `Once` makes the double call safe.
    let crash_log = config
        .db_path
        .parent()
        .map(|d| d.join("songplayer-panic.log"))
        .unwrap_or_else(|| PathBuf::from("songplayer-panic.log"));
    crate::install_panic_hook(crash_log);

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

    // Self-heal stored metadata: re-run the emoji sanitizer over every
    // song/artist value written before it was centralized in
    // metadata::get_metadata (#135). Non-fatal on error.
    if let Err(e) = startup::self_heal_emoji_metadata(&pool).await {
        tracing::warn!("self-heal emoji metadata failed (non-fatal): {e}");
    }

    // Self-heal stored metadata: repair rows whose `song` was written
    // empty by a since-fixed metadata bug (#136) — re-derive song+artist
    // from the stored title. Non-fatal on error.
    if let Err(e) = startup::self_heal_empty_song_metadata(&pool).await {
        tracing::warn!("self-heal empty-song metadata failed (non-fatal): {e}");
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
    let presenter_client = presenter::build_from_settings(&pool).await?;

    // 3b. NDI health registry — constructed before AppState so both the engine
    // (writer) and the AppState (reader) can hold an Arc to the same instance.
    let ndi_health_registry = Arc::new(playback::ndi_health::NdiHealthRegistry::new());
    // #151: one burn-id toggle registry shared by AppState (API) + the engine
    // (pipeline spawn + health). Default OFF, never persisted.
    let ndi_burn_registry = Arc::new(playback::ndi_burn::NdiBurnRegistry::new());

    // #51: shared LAN sp.local status — the mDNS task (spawned after AppState)
    // writes it, `/api/v1/status` reads it. Starts empty until the task
    // detects the LAN IP and registers the record.
    let lan_status = mdns::new_status_handle();

    // 3b'. dantesync clock health (#146). Shared handle: the 1 Hz poller writes
    // it, the playback engine reads it into every NDI health snapshot. Never
    // blocks playback — a missing endpoint just reads `no dantesync`.
    let clock_health = Arc::new(std::sync::RwLock::new(
        playback::clock_health::ClockHealth::default(),
    ));
    {
        let url = std::env::var("DANTESYNC_STATUS_URL")
            .unwrap_or_else(|_| playback::clock_health::DANTESYNC_STATUS_URL_DEFAULT.to_string());
        playback::clock_health::spawn_clock_health_poller(
            reqwest::Client::new(),
            url,
            clock_health.clone(),
            shutdown_tx.subscribe(),
        );
    }

    // 3c. Resolume registry — must be created before AppState so the Arc can
    // be stored in state and shared with the health endpoint.
    let resolume_rows =
        sqlx::query("SELECT id, host, port FROM resolume_hosts WHERE is_enabled = 1")
            .fetch_all(&pool)
            .await?;
    let mut resolume_registry_mut = resolume::ResolumeRegistry::new();
    for row in &resolume_rows {
        let host_id: i64 = row.get("id");
        let host: String = row.get("host");
        let port: i32 = row.get("port");
        resolume_registry_mut.add_host(host_id, host, port as u16, shutdown_tx.subscribe());
    }
    let resolume_registry = Arc::new(resolume_registry_mut);

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
        presenter_client: presenter_client.clone(),
        resolume_registry: resolume_registry.clone(),
        ndi_health_registry: ndi_health_registry.clone(),
        ndi_burn_registry: ndi_burn_registry.clone(),
        lan_status: lan_status.clone(),
    };

    // #51: advertise `sp.local` over mDNS so the dashboard stays reachable on
    // the LAN with no internet. Reads `lan_mdns_enabled` (default on); a
    // failure only degrades to no advertisement, it never blocks startup.
    mdns::spawn_lan_mdns(
        pool.clone(),
        config.port,
        lan_status.clone(),
        shutdown_tx.subscribe(),
    )
    .await;

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
    // Same directory as the SQLite DB — where a production operator drops
    // cookies.txt (Netscape format) to authenticate yt-dlp downloads (#141).
    let dl_data_dir = config
        .db_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let dl_shutdown_tx = shutdown_tx.clone();
    let dl_gemini_key = gemini_key.clone();
    let dl_gemini_model = gemini_model.clone();
    let startup_sync_pool = pool.clone();
    let startup_sync_tx = sync_tx.clone();
    let periodic_sync_pool = pool.clone();
    let periodic_sync_tx = sync_tx.clone();
    let periodic_sync_shutdown = shutdown_tx.clone();
    let ytdlp_update_shutdown = shutdown_tx.clone();
    let lyrics_pool = pool.clone();
    let lyrics_cache_dir = config.cache_dir.clone();
    let lyrics_shutdown = shutdown_tx.clone();
    let lyrics_tools_dir = tools_dir;
    let ai_client_for_dl = ai_client.clone();
    // #154 idle gate: the lyrics worker reads these to defer heavy GPU/CPU work
    // while the wall is in use (any pipeline Playing, or OBS streaming/recording).
    let lyrics_ndi_health = ndi_health_registry.clone();
    let lyrics_obs_state = obs_state.clone();
    // #14 karaoke stem worker shares the same tools dir + idle-gate handles.
    let stem_pool = pool.clone();
    let stem_tools_dir = lyrics_tools_dir.clone();
    let stem_ndi_health = ndi_health_registry.clone();
    let stem_obs_state = obs_state.clone();
    let stem_shutdown = shutdown_tx.clone();
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

                // yt-dlp self-update (#140): the download worker never
                // updates its own yt-dlp binary, so a box that has been up
                // for a while silently falls behind YouTube's format
                // changes (observed: `audio download failed … Requested
                // format is not available` on a stale 2026.03 build — see
                // `.claude/rules/youtube-cookies.md`). One-shot update
                // right after tools are ready, then a shutdown-aware
                // periodic re-update — never fatal, a stale yt-dlp should
                // degrade, not crash the server.
                let version_before = tools_mgr.ytdlp_version(&paths.ytdlp).await.ok();
                match tools_mgr.update_ytdlp().await {
                    Ok(()) => {
                        let version_after = tools_mgr.ytdlp_version(&paths.ytdlp).await.ok();
                        info!(
                            version_before = ?version_before,
                            version_after = ?version_after,
                            "yt-dlp self-update: startup check complete"
                        );
                    }
                    Err(e) => warn!("yt-dlp self-update: startup check failed: {e}"),
                }
                // Shared with the download worker below: an update never
                // runs while a song is downloading and vice versa.
                let ytdlp_lock: downloader::YtdlpLock =
                    std::sync::Arc::new(tokio::sync::Mutex::new(()));
                let ytdlp_interval_secs = ytdlp_update_interval_secs();
                tokio::spawn(periodic_ytdlp_update(
                    tools_mgr,
                    paths.ytdlp.clone(),
                    ytdlp_interval_secs,
                    ytdlp_lock.clone(),
                    ytdlp_update_shutdown.subscribe(),
                ));
                info!(
                    interval_secs = ytdlp_interval_secs,
                    "periodic yt-dlp self-update worker started"
                );

                // Defensive self-heal for #40: any normalized=1 row whose
                // FLAC is not at 48 kHz would explode in
                // SplitSyncedDecoder. Flip them back to normalized=0 so
                // the download worker re-normalizes under the post-#38
                // pipeline (which pins -ar 48000 -ac 2).
                if let Err(e) = startup::flip_wrong_sample_rate_rows(
                    &startup_sync_pool,
                    startup::probe_sample_rate_symphonia,
                )
                .await
                {
                    tracing::warn!("self-heal: sample-rate sweep failed: {e}");
                }

                // Startup sync fires AFTER tools are ready so the sync
                // worker doesn't silently drop the requests.
                if let Err(e) =
                    startup::startup_sync_active_playlists(&startup_sync_pool, &startup_sync_tx)
                        .await
                {
                    tracing::warn!("startup sync enqueue failed: {e}");
                }

                // Periodic re-sync (#139): the one-shot startup sync above
                // only ever fires once, so a video added to a YouTube
                // playlist later would never be picked up without an
                // operator manually hitting the sync button. Spawned only
                // once tools are ready, same as the startup sync itself.
                let periodic_interval_secs = playlist_sync_interval_secs();
                tokio::spawn(periodic_playlist_sync(
                    periodic_sync_pool,
                    periodic_sync_tx,
                    periodic_interval_secs,
                    periodic_sync_shutdown.subscribe(),
                ));
                info!(
                    interval_secs = periodic_interval_secs,
                    "periodic playlist sync worker started"
                );

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
                    dl_data_dir,
                    dl_providers,
                    dl_event_tx_for_worker,
                    ytdlp_lock,
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
                    Some(ai_client_for_dl),
                    tools_event_tx.clone(),
                    lyrics_ndi_health,
                    lyrics_obs_state,
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

                // Karaoke stem worker (#14) — separates the catalog into
                // vocals + instrumental sidecars under the SAME #154 idle gate,
                // lowest priority (after lyrics).
                let stem_worker = crate::stems::StemWorker::new(
                    stem_pool,
                    paths.python.clone(),
                    stem_tools_dir,
                    stem_ndi_health,
                    stem_obs_state,
                );
                tokio::spawn(stem_worker.run(stem_shutdown.subscribe()));
                info!("stem worker started");
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
                    "sync request received but tools not yet available, dropping — the periodic sync re-enqueues"
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

    // 9. Resolume command forwarding (registry was built before AppState above).
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

    // #14 karaoke: seed the process-global live control from settings before any
    // pipeline spawns (pipelines read it at song open + per audio chunk).
    crate::stems::control::init_from_settings(&pool).await;

    // 10. Playback engine (bridges API commands to the engine state machine)
    let mut engine = playback::PlaybackEngine::new(playback::PlaybackEngineConfig {
        pool: pool.clone(),
        cache_dir: config.cache_dir.clone(),
        obs_event_tx,
        obs_cmd_tx,
        resolume_tx: resolume_cmd_tx,
        ws_event_tx: event_tx.clone(),
        presenter_client,
        ndi_health_registry,
    });
    // Inject the shared dantesync clock-health handle so every NDI health
    // snapshot carries the current clock state (#146).
    engine.set_clock_health(clock_health);

    // Boundary-paced emission staging flag (#147): DB setting `genlock_pacing`
    // ("true"/"false"), default OFF. Read once before pipelines are spawned.
    let genlock_pacing = db::models::get_setting(&pool, "genlock_pacing")
        .await
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    info!(
        genlock_pacing,
        "genlock boundary-paced emission staging flag"
    );
    engine.set_genlock_pacing(genlock_pacing);
    // #151: share the burn-id toggle registry BEFORE pipelines spawn (each
    // pipeline registers its output into it at spawn).
    engine.set_ndi_burn_registry(ndi_burn_registry.clone());

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

    // Subscribe to RecoveryEvent from the Resolume registry and forward to the
    // engine via EngineCommand::ResolumeRecovered so the engine can re-emit
    // ShowTitle + ShowSubtitles after a host comes back online.
    let mut recovery_rx = resolume_registry.subscribe_recovery();
    let recovery_engine_tx = engine_tx.clone();
    let mut recovery_shutdown = shutdown_tx.subscribe();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                Ok(event) = recovery_rx.recv() => {
                    let _ = recovery_engine_tx.send(EngineCommand::ResolumeRecovered { host: event.host }).await;
                }
                _ = recovery_shutdown.recv() => break,
            }
        }
    });

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
                            // Manual /play from the dashboard. Engine
                            // dispatches resume-vs-scene-on based on
                            // whether Pause captured a snapshot. #88.
                            engine.handle_engine_play(playlist_id).await;
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
                        EngineCommand::PlayVideo { playlist_id, video_id, position_ms } => {
                            engine.handle_play_video(playlist_id, video_id, position_ms).await;
                        }
                        EngineCommand::SceneChanged { playlist_id, on_program } => {
                            // VideosAvailable + SceneOn (on program) or
                            // SceneOff (off program) are folded into
                            // handle_scene_change so every caller goes
                            // through the same sequence.
                            engine.handle_scene_change(playlist_id, on_program).await;
                        }
                        EngineCommand::Seek { playlist_id, position_ms } => {
                            engine.seek(playlist_id, position_ms);
                        }
                        EngineCommand::ResolumeRecovered { host } => {
                            engine.handle_resolume_recovery(&host).await;
                        }
                        EngineCommand::EnsurePipeline { playlist_id } => {
                            // #132: a playlist created/activated at runtime
                            // registers its pipeline the same way startup does.
                            engine.ensure_pipeline_for_playlist(playlist_id).await;
                        }
                        EngineCommand::RemovePipeline { playlist_id } => {
                            // #132: a playlist deleted/deactivated at runtime
                            // tears its pipeline down symmetrically.
                            engine.remove_pipeline(playlist_id);
                        }
                        EngineCommand::SetKaraoke { mode, vocal_gain } => {
                            // #14: update the live control, persist, broadcast,
                            // and reload playing pipelines when the mode changed.
                            engine.set_karaoke(mode, vocal_gain).await;
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

/// Default interval, in seconds, between periodic playlist re-syncs — used
/// whenever `PLAYLIST_SYNC_INTERVAL_SECS` is absent, unparseable, or zero.
const DEFAULT_PLAYLIST_SYNC_INTERVAL_SECS: u64 = 600;

/// Default interval, in seconds, between periodic yt-dlp self-updates
/// (#140) — used whenever `YTDLP_UPDATE_INTERVAL_SECS` is absent,
/// unparseable, or zero.
const DEFAULT_YTDLP_UPDATE_INTERVAL_SECS: u64 = 86400;

/// Pure parser shared by every periodic-interval env override in this
/// module: falls back to `default` on `None`, on a value that doesn't
/// parse as a `u64`, or on `0` — `tokio::time::interval` panics on a
/// zero-duration period, so zero is treated the same as absent.
fn interval_from(env_value: Option<&str>, default: u64) -> u64 {
    match env_value.and_then(|v| v.parse::<u64>().ok()) {
        Some(secs) if secs > 0 => secs,
        _ => default,
    }
}

/// Pure parser for the periodic playlist re-sync interval override.
/// Falls back to [`DEFAULT_PLAYLIST_SYNC_INTERVAL_SECS`] on `None`, on a
/// value that doesn't parse as a `u64`, or on `0`.
fn sync_interval_from(env_value: Option<&str>) -> u64 {
    interval_from(env_value, DEFAULT_PLAYLIST_SYNC_INTERVAL_SECS)
}

/// Read `PLAYLIST_SYNC_INTERVAL_SECS` from the environment, warning (but
/// still falling back to the default) when the value is present but
/// invalid, so a typo in an operator's env file is visible in the logs
/// instead of silently defaulting.
fn playlist_sync_interval_secs() -> u64 {
    let raw = std::env::var("PLAYLIST_SYNC_INTERVAL_SECS").ok();
    if let Some(v) = &raw {
        let valid = v.parse::<u64>().is_ok_and(|n| n > 0);
        if !valid {
            warn!(
                value = %v,
                default_secs = DEFAULT_PLAYLIST_SYNC_INTERVAL_SECS,
                "PLAYLIST_SYNC_INTERVAL_SECS invalid or zero, using default"
            );
        }
    }
    sync_interval_from(raw.as_deref())
}

/// Periodically re-enqueue a [`SyncRequest`] for every active playlist
/// (#139): the one-shot startup sync only ever fires once, so a video
/// added to a YouTube playlist later would never be picked up otherwise.
/// The first `interval.tick()` resolves immediately — that tick is
/// deliberately consumed and discarded before entering the loop, since the
/// startup sync already covered t=0. Exits on shutdown broadcast.
async fn periodic_playlist_sync(
    pool: SqlitePool,
    sync_tx: mpsc::Sender<SyncRequest>,
    interval_secs: u64,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await; // immediate first tick — startup sync already covered it
    loop {
        tokio::select! {
            _ = shutdown.recv() => return,
            _ = interval.tick() => {
                match startup::enqueue_sync_all_active(&pool, &sync_tx).await {
                    Ok(count) => info!(
                        count,
                        "periodic sync: enqueueing one SyncRequest per active playlist"
                    ),
                    Err(e) => warn!("periodic sync enqueue failed: {e}"),
                }
            }
        }
    }
}

/// Read `YTDLP_UPDATE_INTERVAL_SECS` from the environment (#140), warning
/// (but still falling back to the default) when the value is present but
/// invalid — same shape as [`playlist_sync_interval_secs`].
fn ytdlp_update_interval_secs() -> u64 {
    let raw = std::env::var("YTDLP_UPDATE_INTERVAL_SECS").ok();
    if let Some(v) = &raw {
        let valid = v.parse::<u64>().is_ok_and(|n| n > 0);
        if !valid {
            warn!(
                value = %v,
                default_secs = DEFAULT_YTDLP_UPDATE_INTERVAL_SECS,
                "YTDLP_UPDATE_INTERVAL_SECS invalid or zero, using default"
            );
        }
    }
    interval_from(raw.as_deref(), DEFAULT_YTDLP_UPDATE_INTERVAL_SECS)
}

/// Periodically re-run `yt-dlp --update` (#140): the download worker never
/// updates its own yt-dlp binary, so a long-running box falls behind
/// YouTube's format changes over time (see
/// `.claude/rules/youtube-cookies.md`). The startup one-shot update
/// already covers t=0, so the first `interval.tick()` is deliberately
/// consumed and discarded before entering the loop, same pattern as
/// [`periodic_playlist_sync`]. Never fatal — a failed update just leaves
/// the current binary in place. Exits on shutdown broadcast.
async fn periodic_ytdlp_update(
    tools_mgr: downloader::tools::ToolsManager,
    ytdlp_path: PathBuf,
    interval_secs: u64,
    ytdlp_lock: downloader::YtdlpLock,
    mut shutdown: broadcast::Receiver<()>,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await; // immediate first tick — startup update already covered it
    loop {
        tokio::select! {
            _ = shutdown.recv() => return,
            _ = interval.tick() => {
                // Wait for any in-flight download before touching the binary.
                let _ytdlp_guard = ytdlp_lock.lock().await;
                match tools_mgr.update_ytdlp().await {
                    Ok(()) => {
                        let version = tools_mgr.ytdlp_version(&ytdlp_path).await.ok();
                        info!(version = ?version, "periodic yt-dlp self-update succeeded");
                    }
                    Err(e) => warn!("periodic yt-dlp self-update failed: {e}"),
                }
            }
        }
    }
}

#[cfg(test)]
mod lib_tests;
