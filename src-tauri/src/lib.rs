//! SongPlayer Tauri desktop shell library.
//!
//! Embeds `sp-server` and runs it in the background while providing
//! a system-tray icon and a WebView window pointing at the dashboard.

use std::path::{Path, PathBuf};

use tauri::Manager;
use tracing_appender::rolling::{RollingFileAppender, Rotation};

mod tray;
mod tray_icons;

/// Run the Tauri application.
pub fn run() {
    // Hold the worker guard for the process lifetime so the background log
    // writer thread keeps draining buffered messages until shutdown.
    let _log_guard = setup_logging();

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

/// Build the rolling file appender used for `songplayer.log.<DATE>` files.
///
/// Daily rotation, retains the last 14 files. Always opens in append mode —
/// previous-session logs are NEVER truncated. This is critical for diagnosing
/// failures where the operator restarts the app before logs can be collected
/// (e.g. the 2026-04-27 dark-wall incident, where the morning's failure log
/// was destroyed when the process restarted).
fn build_file_appender(log_dir: &Path) -> std::io::Result<RollingFileAppender> {
    RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("songplayer")
        .filename_suffix("log")
        .max_log_files(14)
        .build(log_dir)
        .map_err(|e| std::io::Error::other(e.to_string()))
}

fn setup_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,sp_server=debug"));

    // Log to file so we can diagnose issues on win-resolume across restarts.
    let log_dir = data_directory();
    let _ = std::fs::create_dir_all(&log_dir);

    let (file_layer, guard) = match build_file_appender(&log_dir) {
        Ok(appender) => {
            let (non_blocking, guard) = tracing_appender::non_blocking(appender);
            let layer = fmt::layer()
                .with_target(true)
                .with_ansi(false)
                .with_writer(non_blocking);
            (Some(layer), Some(guard))
        }
        Err(_) => (None, None),
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(true))
        .with(file_layer)
        .init();

    guard
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for the 2026-04-27 dark-wall log-loss incident.
    ///
    /// Two consecutive sessions in the same log directory must both end up in
    /// the on-disk log file. The previous implementation used `File::create`
    /// which truncates, destroying the prior session's evidence.
    #[test]
    fn rolling_appender_preserves_previous_session_content() {
        use std::io::Write;

        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();

        // Session 1
        {
            let appender = build_file_appender(dir).expect("build session 1");
            let mut w = appender;
            w.write_all(b"session-1-line\n").expect("write session 1");
            w.flush().expect("flush appender");
        }

        // Session 2 — same directory, same prefix.
        {
            let appender = build_file_appender(dir).expect("build session 2");
            let mut w = appender;
            w.write_all(b"session-2-line\n").expect("write session 2");
            w.flush().expect("flush appender");
        }

        // Daily rotation means both writes go into the same dated file.
        let entries: Vec<_> = std::fs::read_dir(dir)
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("songplayer"))
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "expected exactly one rolled file under daily rotation, got {}",
            entries.len()
        );
        let content = std::fs::read_to_string(entries[0].path()).expect("read log");
        assert!(
            content.contains("session-1-line"),
            "session 1 content was truncated: {content:?}"
        );
        assert!(
            content.contains("session-2-line"),
            "session 2 content missing: {content:?}"
        );
    }
}
