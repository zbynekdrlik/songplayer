//! System tray icon and menu setup.

use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};
use tokio::sync::broadcast;

use crate::tray_icons;

pub fn setup_tray(
    app: &AppHandle,
    shutdown_tx: broadcast::Sender<()>,
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
                let _ = shutdown_tx.send(());
                std::thread::sleep(std::time::Duration::from_millis(500));
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
