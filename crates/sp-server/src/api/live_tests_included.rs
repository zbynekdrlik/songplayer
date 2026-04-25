//! Tests for `api::live` handlers. Included as a sibling file to keep
//! `live.rs` focused and under any file-size cap.

use crate::AppState;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use sqlx::SqlitePool;
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast, mpsc};
use tower::ServiceExt;

async fn setup() -> (SqlitePool, i64, i64, i64) {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    crate::startup::ensure_live_playlist_exists(&pool)
        .await
        .unwrap();

    let yt = crate::db::models::insert_playlist(&pool, "src", "https://yt.com/src")
        .await
        .unwrap();
    let v1 = crate::db::models::upsert_video(&pool, yt.id, "a", Some("A"))
        .await
        .unwrap()
        .id;
    let v2 = crate::db::models::upsert_video(&pool, yt.id, "b", Some("B"))
        .await
        .unwrap()
        .id;
    sqlx::query("UPDATE videos SET normalized = 1 WHERE id IN (?, ?)")
        .bind(v1)
        .bind(v2)
        .execute(&pool)
        .await
        .unwrap();

    let ytlive_id: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name='ytlive'")
        .fetch_one(&pool)
        .await
        .unwrap();
    (pool, ytlive_id, v1, v2)
}

fn build_state(pool: SqlitePool, engine_tx: mpsc::Sender<crate::EngineCommand>) -> AppState {
    let (event_tx, _) = broadcast::channel(16);
    let (sync_tx, _) = mpsc::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (obs_rebuild_tx, _) = broadcast::channel(4);
    AppState {
        pool,
        event_tx,
        engine_tx,
        obs_state: Arc::new(RwLock::new(crate::obs::ObsState::default())),
        tools_status: Arc::new(RwLock::new(crate::ToolsStatus::default())),
        tool_paths: Arc::new(RwLock::new(None)),
        sync_tx,
        resolume_tx,
        obs_rebuild_tx,
        cache_dir: std::path::PathBuf::from("/tmp/cache"),
        ai_proxy: Arc::new(crate::ai::proxy::ProxyManager::new(
            std::path::PathBuf::from("/tmp/cache"),
            crate::ai::proxy::ProxyManager::default_port(),
        )),
        ai_client: Arc::new(crate::ai::client::AiClient::new(
            crate::ai::AiSettings::default(),
        )),
        presenter_client: None,
    }
}

#[tokio::test]
async fn post_item_appends_and_returns_position() {
    let (pool, ytlive_id, v1, _) = setup().await;
    let (engine_tx, _engine_rx) = mpsc::channel(8);
    let app = crate::api::router(build_state(pool.clone(), engine_tx), None);

    let body = format!(r#"{{"video_id": {v1}}}"#);
    let resp = app
        .oneshot(
            Request::post(format!("/api/v1/playlists/{ytlive_id}/items"))
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["position"], 0);
}

#[tokio::test]
async fn post_item_on_youtube_playlist_returns_409() {
    let (pool, _, v1, _) = setup().await;
    let yt_id: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name='src'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let (engine_tx, _) = mpsc::channel(8);
    let app = crate::api::router(build_state(pool, engine_tx), None);

    let body = format!(r#"{{"video_id": {v1}}}"#);
    let resp = app
        .oneshot(
            Request::post(format!("/api/v1/playlists/{yt_id}/items"))
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn delete_item_returns_ok_and_removes_row() {
    let (pool, ytlive_id, v1, _) = setup().await;
    crate::db::models::append_playlist_item(&pool, ytlive_id, v1)
        .await
        .unwrap();

    let (engine_tx, _) = mpsc::channel(8);
    let app = crate::api::router(build_state(pool.clone(), engine_tx), None);

    let resp = app
        .oneshot(
            Request::delete(format!("/api/v1/playlists/{ytlive_id}/items/{v1}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let items = crate::db::models::list_playlist_items(&pool, ytlive_id)
        .await
        .unwrap();
    assert!(items.is_empty());
}

#[tokio::test]
async fn get_items_returns_list_in_order() {
    let (pool, ytlive_id, v1, v2) = setup().await;
    crate::db::models::append_playlist_item(&pool, ytlive_id, v1)
        .await
        .unwrap();
    crate::db::models::append_playlist_item(&pool, ytlive_id, v2)
        .await
        .unwrap();

    let (engine_tx, _) = mpsc::channel(8);
    let app = crate::api::router(build_state(pool, engine_tx), None);

    let resp = app
        .oneshot(
            Request::get(format!("/api/v1/playlists/{ytlive_id}/items"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let arr: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["position"], 0);
    assert_eq!(arr[0]["video_id"], v1);
    assert_eq!(arr[1]["position"], 1);
    assert_eq!(arr[1]["video_id"], v2);
}

#[tokio::test]
async fn play_video_sends_engine_command() {
    let (pool, ytlive_id, v1, _) = setup().await;
    crate::db::models::append_playlist_item(&pool, ytlive_id, v1)
        .await
        .unwrap();

    let (engine_tx, mut engine_rx) = mpsc::channel(8);
    let app = crate::api::router(build_state(pool, engine_tx), None);

    let body = format!(r#"{{"video_id": {v1}}}"#);
    let resp = app
        .oneshot(
            Request::post(format!("/api/v1/playlists/{ytlive_id}/play-video"))
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let cmd = engine_rx.recv().await.expect("engine command");
    match cmd {
        crate::EngineCommand::PlayVideo {
            playlist_id,
            video_id,
        } => {
            assert_eq!(playlist_id, ytlive_id);
            assert_eq!(video_id, v1);
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

/// play-video against a youtube-kind playlist must return 409, not silently
/// dispatch an engine command. Protects the 404/409 status-code discipline
/// of the sibling handlers.
#[tokio::test]
async fn play_video_on_youtube_playlist_returns_409() {
    let (pool, _, v1, _) = setup().await;
    let yt_id: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name='src'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let (engine_tx, mut engine_rx) = mpsc::channel(8);
    let app = crate::api::router(build_state(pool, engine_tx), None);

    let body = format!(r#"{{"video_id": {v1}}}"#);
    let resp = app
        .oneshot(
            Request::post(format!("/api/v1/playlists/{yt_id}/play-video"))
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    // No engine command should have been dispatched.
    assert!(engine_rx.try_recv().is_err());
}

/// play-video with a video_id that isn't in the set list must return 404,
/// not dispatch the engine command. Prevents a client from triggering
/// arbitrary playback via the custom-playlist URL.
#[tokio::test]
async fn play_video_with_unknown_video_returns_404() {
    let (pool, ytlive_id, v1, _) = setup().await;
    // v1 exists as a video but is NOT in the ytlive set list.
    let (engine_tx, mut engine_rx) = mpsc::channel(8);
    let app = crate::api::router(build_state(pool, engine_tx), None);

    let body = format!(r#"{{"video_id": {v1}}}"#);
    let resp = app
        .oneshot(
            Request::post(format!("/api/v1/playlists/{ytlive_id}/play-video"))
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(engine_rx.try_recv().is_err());
}

/// Move an item one slot down — it must swap with the next neighbour.
/// Starting order: [v1, v2]. After POST move v1 down → [v2, v1].
#[tokio::test]
async fn move_item_down_swaps_with_next_neighbour() {
    let (pool, ytlive_id, v1, v2) = setup().await;
    crate::db::models::append_playlist_item(&pool, ytlive_id, v1)
        .await
        .unwrap();
    crate::db::models::append_playlist_item(&pool, ytlive_id, v2)
        .await
        .unwrap();

    let (engine_tx, _) = mpsc::channel(8);
    let app = crate::api::router(build_state(pool.clone(), engine_tx), None);

    let resp = app
        .oneshot(
            Request::post(format!("/api/v1/playlists/{ytlive_id}/items/{v1}/move"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"direction":"down"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let items = crate::db::models::list_playlist_items(&pool, ytlive_id)
        .await
        .unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(
        items[0].video_id, v2,
        "v2 should be first after v1 moves down"
    );
    assert_eq!(items[1].video_id, v1, "v1 should be second");
}

/// Move up at the top is a no-op — must return OK and leave order intact.
#[tokio::test]
async fn move_item_up_at_top_is_noop() {
    let (pool, ytlive_id, v1, v2) = setup().await;
    crate::db::models::append_playlist_item(&pool, ytlive_id, v1)
        .await
        .unwrap();
    crate::db::models::append_playlist_item(&pool, ytlive_id, v2)
        .await
        .unwrap();

    let (engine_tx, _) = mpsc::channel(8);
    let app = crate::api::router(build_state(pool.clone(), engine_tx), None);

    let resp = app
        .oneshot(
            Request::post(format!("/api/v1/playlists/{ytlive_id}/items/{v1}/move"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"direction":"up"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let items = crate::db::models::list_playlist_items(&pool, ytlive_id)
        .await
        .unwrap();
    assert_eq!(items[0].video_id, v1);
    assert_eq!(items[1].video_id, v2);
}

/// Unknown direction must 400, not silently succeed or 500.
#[tokio::test]
async fn move_item_invalid_direction_returns_400() {
    let (pool, ytlive_id, v1, _) = setup().await;
    crate::db::models::append_playlist_item(&pool, ytlive_id, v1)
        .await
        .unwrap();

    let (engine_tx, _) = mpsc::channel(8);
    let app = crate::api::router(build_state(pool, engine_tx), None);

    let resp = app
        .oneshot(
            Request::post(format!("/api/v1/playlists/{ytlive_id}/items/{v1}/move"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"direction":"sideways"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// play-video against a playlist that doesn't exist at all must return 404.
#[tokio::test]
async fn play_video_on_missing_playlist_returns_404() {
    let (pool, _, v1, _) = setup().await;
    let (engine_tx, mut engine_rx) = mpsc::channel(8);
    let app = crate::api::router(build_state(pool, engine_tx), None);

    let body = format!(r#"{{"video_id": {v1}}}"#);
    let resp = app
        .oneshot(
            Request::post("/api/v1/playlists/99999/play-video")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(engine_rx.try_recv().is_err());
}
