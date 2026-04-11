//! HTTP API and WebSocket — Axum router, REST endpoints, and dashboard WebSocket.

pub mod routes;
pub mod websocket;

use std::path::PathBuf;

use axum::Router;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};

use crate::AppState;

/// Build the Axum router with all API routes.
///
/// If `dist_dir` is provided, serves the WASM frontend as a SPA fallback.
pub fn router(state: AppState, dist_dir: Option<PathBuf>) -> Router {
    let mut app = Router::new()
        // Playlists
        .route(
            "/api/v1/playlists",
            axum::routing::get(routes::list_playlists).post(routes::create_playlist),
        )
        .route(
            "/api/v1/playlists/{id}",
            axum::routing::get(routes::get_playlist)
                .put(routes::update_playlist)
                .delete(routes::delete_playlist),
        )
        .route(
            "/api/v1/playlists/{id}/sync",
            axum::routing::post(routes::sync_playlist),
        )
        .route(
            "/api/v1/playlists/{id}/videos",
            axum::routing::get(routes::list_videos),
        )
        // Playback
        .route(
            "/api/v1/playback/{playlist_id}/play",
            axum::routing::post(routes::play),
        )
        .route(
            "/api/v1/playback/{playlist_id}/pause",
            axum::routing::post(routes::pause),
        )
        .route(
            "/api/v1/playback/{playlist_id}/skip",
            axum::routing::post(routes::skip),
        )
        .route(
            "/api/v1/playback/{playlist_id}/previous",
            axum::routing::post(routes::previous),
        )
        .route(
            "/api/v1/playback/{playlist_id}/mode",
            axum::routing::put(routes::set_mode),
        )
        // Settings
        .route(
            "/api/v1/settings",
            axum::routing::get(routes::get_settings).patch(routes::update_settings),
        )
        // Status
        .route("/api/v1/status", axum::routing::get(routes::status))
        // Resolume hosts
        .route(
            "/api/v1/resolume/hosts",
            axum::routing::get(routes::list_resolume_hosts).post(routes::add_resolume_host),
        )
        .route(
            "/api/v1/resolume/hosts/{id}",
            axum::routing::delete(routes::delete_resolume_host),
        )
        // WebSocket
        .route("/api/v1/ws", axum::routing::get(websocket::ws_handler))
        // Middleware
        .layer(CorsLayer::permissive())
        .with_state(state);

    // Serve WASM frontend as SPA if dist_dir is provided.
    if let Some(dist) = dist_dir {
        let index = dist.join("index.html");
        app = app.fallback_service(ServeDir::new(&dist).fallback(ServeFile::new(index)));
    }

    app
}
