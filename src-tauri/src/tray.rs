//! System tray icon and menu setup.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};
use tokio::runtime::Handle as RuntimeHandle;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::tray_icons;

/// Max time the tray Exit handler waits for the server task to finish
/// draining workers / closing NDI / closing OBS WS / flushing DB before
/// it terminates the process anyway. 30 s is longer than every documented
/// per-subsystem cleanup step in #81 combined.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

pub fn setup_tray(
    app: &AppHandle,
    shutdown_tx: broadcast::Sender<()>,
    runtime_handle: RuntimeHandle,
    server_join: Arc<Mutex<Option<JoinHandle<()>>>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let version_label = format!("SongPlayer v{}", env!("BUILD_VERSION"));
    let version_item = MenuItem::with_id(app, "version", &version_label, false, None::<&str>)?;

    let separator1 = PredefinedMenuItem::separator(app)?;

    let open_item = MenuItem::with_id(app, "open", "Open Dashboard", true, None::<&str>)?;

    // Get LAN IP for copy URL.
    let ip = local_ip_address::local_ip()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string());
    let dashboard_url = format!("{}:{}", ip, sp_core::config::DEFAULT_API_PORT);
    let copy_url_label = format!("Copy URL ({})", dashboard_url);
    let copy_url_item = MenuItem::with_id(app, "copy_url", &copy_url_label, true, None::<&str>)?;

    let separator2 = PredefinedMenuItem::separator(app)?;
    let quit_item = MenuItem::with_id(app, "quit", "Exit", true, None::<&str>)?;

    let menu = Menu::with_items(
        app,
        &[
            &version_item,
            &separator1,
            &open_item,
            &copy_url_item,
            &separator2,
            &quit_item,
        ],
    )?;

    let icon = tray_icons::make_idle_icon();

    // Clone dashboard_url for the menu event closure.
    let dashboard_url_for_copy = dashboard_url.clone();

    let _tray = TrayIconBuilder::new()
        .icon(Image::new_owned(icon.data, icon.width, icon.height))
        .menu(&menu)
        .tooltip("SongPlayer")
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "open" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
            "copy_url" => {
                use tauri_plugin_clipboard_manager::ClipboardExt;
                let _ = app.clipboard().write_text(&dashboard_url_for_copy);
            }
            "quit" => {
                // #81: broadcast shutdown, then BLOCK on the server task
                // join with a real wall-clock timeout. The previous code
                // slept a fixed 500 ms and then called app.exit(0)
                // regardless — that aborted NDI sender Drop, in-flight
                // OBS WebSocket close, Replicate API calls, and the
                // sqlx pool drain. Combined with #80 that left the next
                // restart in a stale-network state.
                let _ = shutdown_tx.send(());
                if let Some(handle) = server_join.lock().ok().and_then(|mut g| g.take()) {
                    let ok = runtime_handle.block_on(sp_server::shutdown::await_server_join(
                        handle,
                        SHUTDOWN_TIMEOUT,
                    ));
                    if !ok {
                        tracing::warn!(
                            "server task did not finish within {SHUTDOWN_TIMEOUT:?}; exiting anyway"
                        );
                    }
                }
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
        })
        .build(app)?;

    Ok(())
}
