//! HTTP API and WebSocket — Axum router, REST endpoints, and dashboard WebSocket.

pub mod ai;
pub mod live;
pub mod lyrics;
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
        // Lyrics
        .route(
            "/api/v1/videos/{id}/lyrics",
            axum::routing::get(routes::get_video_lyrics),
        )
        .route(
            "/api/v1/videos/{id}/lyrics/reprocess",
            axum::routing::post(routes::reprocess_video_lyrics),
        )
        .route(
            "/api/v1/lyrics/status",
            axum::routing::get(routes::get_lyrics_status),
        )
        .route(
            "/api/v1/lyrics/queue",
            axum::routing::get(lyrics::get_queue),
        )
        .route(
            "/api/v1/lyrics/songs",
            axum::routing::get(lyrics::list_songs),
        )
        .route(
            "/api/v1/lyrics/songs/{video_id}",
            axum::routing::get(lyrics::get_song_detail),
        )
        .route(
            "/api/v1/lyrics/reprocess",
            axum::routing::post(lyrics::post_reprocess),
        )
        .route(
            "/api/v1/lyrics/reprocess-all-stale",
            axum::routing::post(lyrics::post_reprocess_all_stale),
        )
        .route(
            "/api/v1/lyrics/clear-manual-queue",
            axum::routing::post(lyrics::post_clear_manual),
        )
        // WebSocket
        .route("/api/v1/ws", axum::routing::get(websocket::ws_handler))
        // AI proxy
        .route(
            "/api/v1/ai/proxy/start",
            axum::routing::post(ai::proxy_start),
        )
        .route("/api/v1/ai/proxy/stop", axum::routing::post(ai::proxy_stop))
        .route(
            "/api/v1/ai/proxy/login",
            axum::routing::post(ai::proxy_login),
        )
        .route(
            "/api/v1/ai/proxy/complete-login",
            axum::routing::post(ai::proxy_complete_login),
        )
        .route("/api/v1/ai/status", axum::routing::get(ai::ai_status))
        // Custom playlist set list + click-to-play.
        .route(
            "/api/v1/playlists/{id}/items",
            axum::routing::get(live::get_items).post(live::post_add_item),
        )
        .route(
            "/api/v1/playlists/{id}/items/{video_id}",
            axum::routing::delete(live::delete_item),
        )
        .route(
            "/api/v1/playlists/{id}/play-video",
            axum::routing::post(live::post_play_video),
        )
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
