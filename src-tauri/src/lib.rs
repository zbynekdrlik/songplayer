//! SongPlayer Tauri desktop shell library.
//!
//! Embeds `sp-server` and runs it in the background while providing
//! a system-tray icon and a WebView window pointing at the dashboard.

use std::path::PathBuf;

use tauri::Manager;

mod tray;
mod tray_icons;

/// Run the Tauri application.
pub fn run() {
    setup_logging();

    tracing::info!("SongPlayer v{} starting", env!("BUILD_VERSION"));

    // Create Tokio runtime for sp-server.
    let runtime = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");

    // Determine data directory.
    let data_dir = data_directory();
    std::fs::create_dir_all(&data_dir).expect("Failed to create data directory");

    // Create shutdown channel.
    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);
    let shutdown_tx_clone = shutdown_tx.clone();

    // Spawn sp-server in background.
    let server_data_dir = data_dir.clone();
    let server_shutdown = shutdown_tx.subscribe();
    runtime.spawn(async move {
        // Look for dist/ next to the executable (NSIS install puts it there).
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()));
        let dist_dir = exe_dir
            .map(|d| d.join("dist"))
            .filter(|d| d.join("index.html").exists());

        let config = sp_server::ServerConfig {
            db_path: server_data_dir.join("songplayer.db"),
            cache_dir: server_data_dir.join("cache"),
            port: sp_core::config::DEFAULT_API_PORT,
            dist_dir,
        };
        if let Err(e) = sp_server::start(config, server_shutdown).await {
            tracing::error!("Server error: {e}");
        }
    });

    // Keep runtime alive in background thread.
    let _runtime_guard = std::thread::spawn(move || {
        runtime.block_on(async {
            tokio::signal::ctrl_c().await.ok();
        });
    });

    // Build Tauri app.
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // Focus existing window on second launch attempt.
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(move |app| {
            // Setup tray icon.
            tray::setup_tray(app.handle(), shutdown_tx_clone)?;

            // Hide window on close (minimize to tray).
            let window = app.get_webview_window("main").unwrap();
            let window_clone = window.clone();
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window_clone.hide();
                }
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn data_directory() -> PathBuf {
    // Windows: C:\ProgramData\SongPlayer
    // Linux: ~/.local/share/songplayer
    #[cfg(windows)]
    {
        PathBuf::from(r"C:\ProgramData\SongPlayer")
    }
    #[cfg(not(windows))]
    {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("songplayer")
    }
}

fn setup_logging() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,sp_server=debug"));

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(true))
        .init();
}
