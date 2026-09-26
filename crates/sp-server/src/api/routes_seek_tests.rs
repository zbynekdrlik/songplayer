//! Axum integration tests for the unified seek route (#194 ROUND 1). Sibling of
//! `routes_seek.rs` so that file stays small and the handler is exercised end to
//! end through the real router.
#![allow(unused_imports)]

use crate::AppState;
use crate::api::router;
use crate::db;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast, mpsc};
use tower::ServiceExt;

/// Build an AppState whose engine receiver is RETAINED so a forwarded
/// `EngineCommand::Seek` can be asserted.
async fn state_with_engine() -> (AppState, mpsc::Receiver<crate::EngineCommand>) {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    let (event_tx, _) = broadcast::channel(16);
    let (engine_tx, engine_rx) = mpsc::channel(16);
    let (sync_tx, _) = mpsc::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (obs_rebuild_tx, _) = broadcast::channel(4);
    let state = AppState {
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
        ai_proxy: std::sync::Arc::new(crate::ai::proxy::ProxyManager::new(
            std::path::PathBuf::from("/tmp/cache"),
            crate::ai::proxy::ProxyManager::default_port(),
        )),
        ai_client: std::sync::Arc::new(crate::ai::client::AiClient::new(
            crate::ai::AiSettings::default(),
        )),
        presenter_client: None,
        resolume_registry: Arc::new(crate::resolume::ResolumeRegistry::new()),
        ndi_health_registry: Arc::new(crate::playback::ndi_health::NdiHealthRegistry::new()),
        ndi_burn_registry: Arc::new(crate::playback::ndi_burn::NdiBurnRegistry::new()),
        preview_registry: Arc::new(crate::playback::preview::PreviewRegistry::new()),
        program_bus: Arc::new(crate::playback::program_bus::ProgramBus::new()),
        lan_status: crate::mdns::new_status_handle(),
    };
    (state, engine_rx)
}

async fn insert_playlist(pool: &sqlx::SqlitePool, id: i64) {
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name) VALUES (?, 'p', 'u', 'n')",
    )
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
}

async fn insert_video(
    pool: &sqlx::SqlitePool,
    id: i64,
    playlist_id: i64,
    duration_ms: Option<i64>,
) {
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, duration_ms) \
         VALUES (?, ?, ?, 1, ?)",
    )
    .bind(id)
    .bind(playlist_id)
    .bind(format!("yt-{id}"))
    .bind(duration_ms)
    .execute(pool)
    .await
    .unwrap();
}

fn seek_request(playlist_id: i64, position_ms: u64) -> Request<Body> {
    let body = serde_json::json!({ "position_ms": position_ms });
    Request::builder()
        .method("POST")
        .uri(format!("/api/v1/playback/{playlist_id}/seek"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

#[tokio::test]
async fn seek_unknown_playlist_is_404() {
    // Use a unique id that is never inserted anywhere.
    let pid = 940_001;
    let (state, _rx) = state_with_engine().await;
    let resp = router(state, None)
        .oneshot(seek_request(pid, 1000))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn seek_nothing_playing_is_409() {
    let pid = 940_002;
    let (state, _rx) = state_with_engine().await;
    insert_playlist(&state.pool, pid).await;
    // Ensure nothing is registered as playing for this playlist.
    crate::now_playing::global().clear(pid);
    let resp = router(state, None)
        .oneshot(seek_request(pid, 1000))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn seek_forwards_clamped_position_to_engine() {
    let pid = 940_003;
    let vid = 9_400_031;
    let (state, mut rx) = state_with_engine().await;
    insert_playlist(&state.pool, pid).await;
    insert_video(&state.pool, vid, pid, Some(200_000)).await;
    crate::now_playing::global().set(pid, vid);

    // Request a position PAST the song's duration → clamped to 200_000.
    let resp = router(state, None)
        .oneshot(seek_request(pid, 500_000))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let cmd = rx.recv().await.expect("engine must receive a command");
    match cmd {
        crate::EngineCommand::Seek {
            playlist_id,
            position_ms,
        } => {
            assert_eq!(playlist_id, pid);
            assert_eq!(position_ms, 200_000, "position past duration must clamp");
        }
        other => panic!("expected Seek, got {other:?}"),
    }
    crate::now_playing::global().clear(pid);
}

#[tokio::test]
async fn seek_zero_duration_forwards_unclamped() {
    // #198 item 7: a duration of 0 means "unknown / not yet probed". The route
    // used to forward the requested position VERBATIM in that case (no upper
    // clamp bound), so an arbitrary client position reached EngineCommand::Seek
    // unclamped — the vulnerability this fixes. Without a known duration there is
    // no safe bound, so the route now refuses with 409 (a playing, seekable song
    // has a known duration; an un-probed one is not safely seekable). This
    // replaces the old `..._treated_as_unknown_no_clamp` test, which asserted the
    // vulnerable forward-verbatim behaviour. The `duration_ms > 0` filter still
    // guards the `>= 0` mutant: `>= 0` would make 0 a "known" duration and clamp
    // to 0 → 204, so this 409 assertion kills it.
    let pid = 940_005;
    let vid = 9_400_051;
    let (state, mut rx) = state_with_engine().await;
    insert_playlist(&state.pool, pid).await;
    insert_video(&state.pool, vid, pid, Some(0)).await;
    crate::now_playing::global().set(pid, vid);

    let resp = router(state, None)
        .oneshot(seek_request(pid, 45_000))
        .await
        .unwrap();
    // 0.62.0 release review: `videos.duration_ms` is written only at import/sync
    // (NULL for many playable videos) while the slider is enabled from the
    // pipeline's live duration — a 409 here left an enabled seek bar whose every
    // drag failed forever. Unknown DB duration → forward unclamped (the engine
    // and the decoder bound the seek), never refuse.
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(
        matches!(
            rx.try_recv(),
            Ok(crate::EngineCommand::Seek {
                position_ms: 45_000,
                ..
            })
        ),
        "an unknown duration must forward the requested position unclamped"
    );
    crate::now_playing::global().clear(pid);
}

#[tokio::test]
async fn seek_null_duration_forwards_unclamped() {
    // #198 item 7: a NULL `videos.duration_ms` (not yet probed) is also unknown —
    // no clamp bound, so refuse with 409 rather than forward an unbounded seek.
    let pid = 940_006;
    let vid = 9_400_061;
    let (state, mut rx) = state_with_engine().await;
    insert_playlist(&state.pool, pid).await;
    insert_video(&state.pool, vid, pid, None).await;
    crate::now_playing::global().set(pid, vid);

    let resp = router(state, None)
        .oneshot(seek_request(pid, 45_000))
        .await
        .unwrap();
    // 0.62.0 release review: `videos.duration_ms` is written only at import/sync
    // (NULL for many playable videos) while the slider is enabled from the
    // pipeline's live duration — a 409 here left an enabled seek bar whose every
    // drag failed forever. Unknown DB duration → forward unclamped (the engine
    // and the decoder bound the seek), never refuse.
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(
        matches!(
            rx.try_recv(),
            Ok(crate::EngineCommand::Seek {
                position_ms: 45_000,
                ..
            })
        ),
        "an unknown duration must forward the requested position unclamped"
    );
    crate::now_playing::global().clear(pid);
}

#[tokio::test]
async fn seek_within_bounds_forwards_verbatim() {
    let pid = 940_004;
    let vid = 9_400_041;
    let (state, mut rx) = state_with_engine().await;
    insert_playlist(&state.pool, pid).await;
    insert_video(&state.pool, vid, pid, Some(200_000)).await;
    crate::now_playing::global().set(pid, vid);

    let resp = router(state, None)
        .oneshot(seek_request(pid, 45_000))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let cmd = rx.recv().await.expect("engine must receive a command");
    match cmd {
        crate::EngineCommand::Seek { position_ms, .. } => {
            assert_eq!(position_ms, 45_000);
        }
        other => panic!("expected Seek, got {other:?}"),
    }
    crate::now_playing::global().clear(pid);
}
