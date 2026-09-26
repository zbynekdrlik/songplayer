//! HTTP API and WebSocket — Axum router, REST endpoints, and dashboard WebSocket.

pub mod ai;
pub mod dabing; // #180 dubbing D1
pub mod live;
pub mod lyrics;
pub mod lyrics_catalog;
pub mod mix; // #184 round G — the ONE live mixer console
pub mod mix_apply; // #184 live-first mix apply seam
pub mod preview;
pub mod program; // #209 program bus: GET /api/v1/program + POST /api/v1/program/cut
pub mod routes;
pub mod routes_import; // #180 shared bare-URL import core
pub mod routes_ndi_recover;
pub mod routes_seek; // #194 unified seek route
pub mod stems;
pub mod videos;
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
            axum::routing::get(videos::list_videos),
        )
        .route(
            "/api/v1/videos/{id}",
            axum::routing::patch(routes::patch_video),
        )
        .route(
            "/api/v1/videos/import",
            axum::routing::post(routes::import_video),
        )
        // #180 dubbing D1 — Dabing section + per-video dub toggle/mixer.
        .route("/api/v1/dabing", axum::routing::get(dabing::get_dabing))
        .route(
            "/api/v1/dabing/import",
            axum::routing::post(dabing::import_dabing),
        )
        .route(
            "/api/v1/videos/{id}/dub",
            axum::routing::patch(dabing::patch_dub),
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
        // #194: unified seek — same playback family as play/pause/skip/mode.
        .route(
            "/api/v1/playback/{playlist_id}/seek",
            axum::routing::post(routes_seek::post_seek),
        )
        // #15 part 2: live low-res video preview of the currently-playing song.
        .route(
            "/api/v1/playback/{playlist_id}/preview.jpg",
            axum::routing::get(preview::get_playback_preview),
        )
        // #178: live A/V preview STREAM (fragmented MP4 over WebSocket → MSE).
        .route(
            "/api/v1/playback/{playlist_id}/preview.ws",
            axum::routing::get(preview::get_playback_preview_ws),
        )
        // Settings
        .route(
            "/api/v1/settings",
            axum::routing::get(routes::get_settings).patch(routes::update_settings),
        )
        // #184 round G — the ONE global live mixer console (GET faders + now-
        // playing stems; PATCH any subset of the three faders).
        .route(
            "/api/v1/mix",
            axum::routing::get(mix::get_mix).patch(mix::patch_mix),
        )
        // #177: operator re-enqueue of a song for stem separation.
        .route(
            "/api/v1/stems/{video_id}/enqueue",
            axum::routing::post(stems::enqueue),
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
        .route(
            "/api/v1/resolume/health",
            axum::routing::get(routes::get_resolume_health),
        )
        .route(
            "/api/v1/ndi/health",
            axum::routing::get(routes::get_ndi_health),
        )
        .route(
            "/api/v1/ndi/burn",
            axum::routing::post(routes::set_ndi_burn),
        )
        // #173: operator/verification one-shot dark-wall recovery rung.
        .route(
            "/api/v1/ndi/recover/{playlist_id}",
            axum::routing::post(routes_ndi_recover::post_ndi_recover),
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
            "/api/v1/lyrics/songs/{video_id}/reference-feedback",
            axum::routing::post(lyrics::post_reference_feedback),
        )
        .route(
            "/api/v1/lyrics/songs/{video_id}/reference",
            axum::routing::post(lyrics::post_set_reference),
        )
        .route(
            "/api/v1/lyrics/songs/{video_id}/translation-gender",
            axum::routing::patch(lyrics::patch_translation_gender),
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
        .route(
            "/api/v1/lyrics/reprocess-catalog-with-new-gate",
            axum::routing::post(lyrics_catalog::reprocess_catalog_with_new_gate),
        )
        .route(
            "/api/v1/lyrics/quarantine",
            axum::routing::post(lyrics::quarantine_lyrics),
        )
        .route(
            "/api/v1/lyrics/probe-sources",
            axum::routing::post(lyrics::post_probe_sources),
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
            "/api/v1/playlists/{id}/items/{video_id}/move",
            axum::routing::post(live::post_move_item),
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
