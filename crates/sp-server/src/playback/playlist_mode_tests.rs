//! #225 unit 2: a playlist's playback mode has ONE persisted truth, its
//! `playlists.playback_mode` row. A pipeline starts in its row's mode; a
//! mode change from the dashboard (the REST mode route) or the playlist
//! PATCH writes the row FIRST and then reaches the running engine, which
//! tells the dashboards; a mode the row could not take changes nothing.
//!
//! Real in-memory SQLite, the real router (`crate::api::router`), and the
//! engine fed the API's commands through `engine_dispatch::dispatch`, as
//! `lib.rs` does. What the engine tells the dashboard lives in a
//! process-global record, so these tests use playlist ids no other test uses
//! (22 530-22 539) and read back only their own.

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sp_core::models::Playlist;
use sp_core::playback::{PlaybackMode, PlaybackState, TransportState};
use sp_core::ws::ServerMsg;
use sqlx::SqlitePool;
use tokio::sync::{broadcast, mpsc};
use tower::ServiceExt;

use super::super::{PlaybackEngine, PlaybackEngineConfig};
use crate::api::routes::tests::test_state_with_engine_rx;
use crate::{AppState, EngineCommand};

const ROW_SINGLE: i64 = 22_530;
const SAVED: i64 = 22_531;
const PATCHED: i64 = 22_532;
const UNKNOWN_IN_ROW: i64 = 22_533;
const NO_PIPELINE: i64 = 22_534;
const REFUSED: i64 = 22_535;
const NOT_WRITTEN: i64 = 22_536;
const NO_SUCH_PLAYLIST: i64 = 22_537;

/// The engine under test, the channel it tells the dashboard on, and the
/// Resolume channel's receiver (kept alive by the test).
type Rig = (
    PlaybackEngine,
    broadcast::Receiver<ServerMsg>,
    mpsc::Receiver<crate::resolume::ResolumeCommand>,
);

/// An engine on the API state's DB, as `lib.rs` builds it.
fn engine_on(state: &AppState) -> Rig {
    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, resolume_rx) = mpsc::channel(16);
    let (ws_tx, ws_rx) = broadcast::channel::<ServerMsg>(64);
    let engine = PlaybackEngine::new(PlaybackEngineConfig {
        pool: state.pool.clone(),
        cache_dir: PathBuf::from("/tmp/test-cache"),
        obs_event_tx: obs_tx,
        obs_cmd_tx: None,
        resolume_tx,
        ws_event_tx: ws_tx,
        presenter_client: None,
        ndi_health_registry: state.ndi_health_registry.clone(),
    });
    (engine, ws_rx, resolume_rx)
}

/// A playlist row with an NDI output, so an active one gets a pipeline.
async fn insert(pool: &SqlitePool, id: i64, mode: &str, active: bool) {
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, playback_mode, is_active) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(format!("P{id}"))
    .bind(format!("url-{id}"))
    .bind(format!("SP-{id}"))
    .bind(mode)
    .bind(i64::from(active))
    .execute(pool)
    .await
    .unwrap();
}

/// The row's stored `playback_mode`.
async fn stored(pool: &SqlitePool, id: i64) -> String {
    sqlx::query_scalar("SELECT playback_mode FROM playlists WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// One request through the real router; its status.
async fn send(
    state: &AppState,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> StatusCode {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
    crate::api::router(state.clone(), None)
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap()
        .status()
}

/// Feed the engine every command the API sent, as `lib.rs`'s bridge does.
async fn deliver(engine: &mut PlaybackEngine, engine_rx: &mut mpsc::Receiver<EngineCommand>) {
    while let Ok(cmd) = engine_rx.try_recv() {
        crate::engine_dispatch::dispatch(engine, cmd).await;
    }
}

/// The mode the playlist's pipeline plays (`None`: no pipeline).
fn mode_of(engine: &PlaybackEngine, id: i64) -> Option<PlaybackMode> {
    engine.pipelines.get(&id).map(|pp| pp.mode)
}

/// The states the engine told the open dashboards about `id`.
fn states_told(ws_rx: &mut broadcast::Receiver<ServerMsg>, id: i64) -> Vec<ServerMsg> {
    let mut told = Vec::new();
    while let Ok(msg) = ws_rx.try_recv() {
        if matches!(&msg, ServerMsg::PlaybackStateChanged { playlist_id, .. } if *playlist_id == id)
        {
            told.push(msg);
        }
    }
    told
}

/// What a dashboard connecting now is told about `id`.
async fn replay_of(state: &AppState, id: i64) -> Vec<ServerMsg> {
    crate::api::websocket::on_connect_replay(state)
        .await
        .into_iter()
        .filter(|m| {
            matches!(
                m,
                ServerMsg::NowPlaying { playlist_id, .. }
                | ServerMsg::PlaybackStateChanged { playlist_id, .. } if *playlist_id == id
            )
        })
        .collect()
}

fn idle(playlist_id: i64, mode: PlaybackMode) -> ServerMsg {
    ServerMsg::PlaybackStateChanged {
        playlist_id,
        state: PlaybackState::Idle,
        mode,
        transport: TransportState::Idle,
    }
}

#[tokio::test]
async fn a_pipeline_starts_in_its_rows_mode() {
    let (state, _engine_rx) = test_state_with_engine_rx().await;
    insert(&state.pool, ROW_SINGLE, "single", true).await;
    let (mut engine, _ws_rx, _resolume_rx) = engine_on(&state);

    engine.ensure_pipeline_for_playlist(ROW_SINGLE).await;

    assert_eq!(
        mode_of(&engine, ROW_SINGLE),
        Some(PlaybackMode::Single),
        "the pipeline plays the mode its row holds"
    );
    assert_eq!(
        replay_of(&state, ROW_SINGLE).await,
        vec![idle(ROW_SINGLE, PlaybackMode::Single)],
        "and a new dashboard is told that mode before the engine ever spoke of it"
    );
}

#[tokio::test]
async fn a_dashboard_mode_change_is_saved_and_a_fresh_engine_starts_in_it() {
    let (state, mut engine_rx) = test_state_with_engine_rx().await;
    insert(&state.pool, SAVED, "continuous", true).await;
    let (mut engine, mut ws_rx, _resolume_rx) = engine_on(&state);
    engine.ensure_pipeline_for_playlist(SAVED).await;

    // The operator picks "Jedna skladba" in the Player's mode select.
    let status = send(
        &state,
        "PUT",
        &format!("/api/v1/playback/{SAVED}/mode"),
        Some(serde_json::json!({ "mode": "single" })),
    )
    .await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        stored(&state.pool, SAVED).await,
        "single",
        "the row holds the operator's mode"
    );
    deliver(&mut engine, &mut engine_rx).await;
    assert_eq!(
        mode_of(&engine, SAVED),
        Some(PlaybackMode::Single),
        "the running engine plays it"
    );
    assert_eq!(
        states_told(&mut ws_rx, SAVED),
        vec![idle(SAVED, PlaybackMode::Single)],
        "and the open dashboards are told"
    );

    // A restart: a fresh engine on the same DB, built the way `lib.rs`
    // builds it (the active playlists → the startup senders).
    drop(engine);
    let (mut fresh, _fresh_ws_rx, _fresh_resolume_rx) = engine_on(&state);
    let active: Vec<Playlist> = crate::db::models::get_active_playlists(&state.pool)
        .await
        .unwrap()
        .into_iter()
        .filter(|p| p.id == SAVED)
        .collect();
    fresh.create_startup_senders(&active).await;

    assert_eq!(
        mode_of(&fresh, SAVED),
        Some(PlaybackMode::Single),
        "a restart plays the mode the operator chose"
    );
}

#[tokio::test]
async fn a_patch_of_the_mode_reaches_the_running_engine_and_is_broadcast() {
    let (state, mut engine_rx) = test_state_with_engine_rx().await;
    insert(&state.pool, PATCHED, "continuous", true).await;
    let (mut engine, mut ws_rx, _resolume_rx) = engine_on(&state);
    engine.ensure_pipeline_for_playlist(PATCHED).await;

    let status = send(
        &state,
        "PATCH",
        &format!("/api/v1/playlists/{PATCHED}"),
        Some(serde_json::json!({ "playback_mode": "loop" })),
    )
    .await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(stored(&state.pool, PATCHED).await, "loop");
    deliver(&mut engine, &mut engine_rx).await;
    assert_eq!(
        mode_of(&engine, PATCHED),
        Some(PlaybackMode::Loop),
        "the running engine plays the patched mode"
    );
    assert_eq!(
        states_told(&mut ws_rx, PATCHED),
        vec![idle(PATCHED, PlaybackMode::Loop)],
        "the open dashboards are told it"
    );
    assert_eq!(
        replay_of(&state, PATCHED).await,
        vec![idle(PATCHED, PlaybackMode::Loop)],
        "and so is the next dashboard that connects"
    );
}

#[tokio::test]
async fn an_unknown_mode_in_the_row_plays_the_default() {
    let (state, _engine_rx) = test_state_with_engine_rx().await;
    // Not written by the API (it refuses an unknown mode), only by hand.
    insert(&state.pool, UNKNOWN_IN_ROW, "shuffle", true).await;
    let (mut engine, _ws_rx, _resolume_rx) = engine_on(&state);

    engine.ensure_pipeline_for_playlist(UNKNOWN_IN_ROW).await;

    assert_eq!(
        mode_of(&engine, UNKNOWN_IN_ROW),
        Some(PlaybackMode::Continuous)
    );
    assert_eq!(
        replay_of(&state, UNKNOWN_IN_ROW).await,
        vec![idle(UNKNOWN_IN_ROW, PlaybackMode::Continuous)]
    );
}

#[tokio::test]
async fn a_playlist_with_no_pipeline_is_told_its_new_mode_and_forgotten_when_deleted() {
    let (state, mut engine_rx) = test_state_with_engine_rx().await;
    // Inactive: no pipeline, but the dashboard lists it and its mode select.
    insert(&state.pool, NO_PIPELINE, "continuous", false).await;
    let (mut engine, mut ws_rx, _resolume_rx) = engine_on(&state);

    let status = send(
        &state,
        "PUT",
        &format!("/api/v1/playback/{NO_PIPELINE}/mode"),
        Some(serde_json::json!({ "mode": "loop" })),
    )
    .await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(stored(&state.pool, NO_PIPELINE).await, "loop");
    deliver(&mut engine, &mut engine_rx).await;
    assert_eq!(mode_of(&engine, NO_PIPELINE), None, "still no pipeline");
    assert_eq!(
        states_told(&mut ws_rx, NO_PIPELINE),
        vec![idle(NO_PIPELINE, PlaybackMode::Loop)],
        "the open dashboards are told the new mode"
    );
    assert_eq!(
        replay_of(&state, NO_PIPELINE).await,
        vec![idle(NO_PIPELINE, PlaybackMode::Loop)]
    );

    // Deleted: its record goes with its row.
    let status = send(
        &state,
        "DELETE",
        &format!("/api/v1/playlists/{NO_PIPELINE}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    deliver(&mut engine, &mut engine_rx).await;
    assert_eq!(
        replay_of(&state, NO_PIPELINE).await,
        Vec::<ServerMsg>::new(),
        "a deleted playlist is never replayed"
    );
}

#[tokio::test]
async fn an_unknown_mode_is_refused_and_nothing_changes() {
    let (state, mut engine_rx) = test_state_with_engine_rx().await;
    insert(&state.pool, REFUSED, "single", true).await;
    let (mut engine, mut ws_rx, _resolume_rx) = engine_on(&state);
    engine.ensure_pipeline_for_playlist(REFUSED).await;

    let put = send(
        &state,
        "PUT",
        &format!("/api/v1/playback/{REFUSED}/mode"),
        Some(serde_json::json!({ "mode": "shuffle" })),
    )
    .await;
    let patch = send(
        &state,
        "PATCH",
        &format!("/api/v1/playlists/{REFUSED}"),
        Some(serde_json::json!({ "playback_mode": "shuffle" })),
    )
    .await;

    assert_eq!(put, StatusCode::BAD_REQUEST);
    assert_eq!(patch, StatusCode::BAD_REQUEST);
    assert_eq!(stored(&state.pool, REFUSED).await, "single");
    assert!(engine_rx.try_recv().is_err(), "the engine is told nothing");
    assert_eq!(mode_of(&engine, REFUSED), Some(PlaybackMode::Single));
    assert_eq!(states_told(&mut ws_rx, REFUSED), Vec::<ServerMsg>::new());
}

#[tokio::test]
async fn a_mode_the_row_could_not_take_leaves_the_engines_mode() {
    let (state, mut engine_rx) = test_state_with_engine_rx().await;
    insert(&state.pool, NOT_WRITTEN, "loop", true).await;
    let (mut engine, mut ws_rx, _resolume_rx) = engine_on(&state);
    engine.ensure_pipeline_for_playlist(NOT_WRITTEN).await;
    // The row refuses any mode write (a DB failure).
    sqlx::query(
        "CREATE TRIGGER refuse_mode_22536 BEFORE UPDATE OF playback_mode ON playlists \
         WHEN NEW.id = 22536 BEGIN SELECT RAISE(ABORT, 'the row refuses'); END",
    )
    .execute(&state.pool)
    .await
    .unwrap();

    let status = send(
        &state,
        "PUT",
        &format!("/api/v1/playback/{NOT_WRITTEN}/mode"),
        Some(serde_json::json!({ "mode": "single" })),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "the dashboard is told the change failed"
    );
    assert_eq!(stored(&state.pool, NOT_WRITTEN).await, "loop");
    assert!(
        engine_rx.try_recv().is_err(),
        "the engine is never told a mode the row does not hold"
    );
    assert_eq!(mode_of(&engine, NOT_WRITTEN), Some(PlaybackMode::Loop));
    assert_eq!(
        states_told(&mut ws_rx, NOT_WRITTEN),
        Vec::<ServerMsg>::new()
    );
}

#[tokio::test]
async fn a_mode_for_a_playlist_that_does_not_exist_is_404_and_tells_nobody() {
    let (state, mut engine_rx) = test_state_with_engine_rx().await;

    let status = send(
        &state,
        "PUT",
        &format!("/api/v1/playback/{NO_SUCH_PLAYLIST}/mode"),
        Some(serde_json::json!({ "mode": "loop" })),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(engine_rx.try_recv().is_err());
}
