//! Regression tests for #132: a playlist created / activated / deleted at
//! runtime (via the API) must register / tear down its playback pipeline with
//! the engine, so scene detection can start playback without a process
//! restart. Before the fix, the API CRUD handlers fired only `obs_rebuild_tx`
//! and nothing ever called `ensure_pipeline` at runtime, so a runtime-created
//! playlist logged `no pipeline for playlist` forever until a restart.
//!
//! Sibling `#[path]` file so `playback/mod.rs` stays under the 1000-line
//! airuleset cap.

#![allow(unused_imports)]

use super::*;
use sp_core::ws::ServerMsg;
use tokio::sync::{broadcast, mpsc};

/// Build a bare engine over an in-memory DB with migrations applied — mirrors
/// the construction harness in `playback/tests.rs`.
async fn engine_with_migrated_pool() -> (PlaybackEngine, SqlitePool) {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let (obs_tx, _obs_rx) = broadcast::channel(16);
    let (resolume_tx, _resolume_rx) = mpsc::channel(16);
    let (ws_tx, _ws_rx) = broadcast::channel::<ServerMsg>(16);
    let engine = PlaybackEngine::new(PlaybackEngineConfig {
        pool: pool.clone(),
        cache_dir: std::path::PathBuf::from("/tmp/test-cache"),
        obs_event_tx: obs_tx,
        obs_cmd_tx: None,
        resolume_tx,
        ws_event_tx: ws_tx,
        presenter_client: None,
        ndi_health_registry: std::sync::Arc::new(
            crate::playback::ndi_health::NdiHealthRegistry::new(),
        ),
    });
    (engine, pool)
}

/// #132 core repro: an active playlist created AFTER engine start (the API
/// `create_playlist` path fires only `obs_rebuild_tx`, never registers a
/// pipeline) must get a pipeline once the engine is told to ensure it.
/// RED: `ensure_pipeline_for_playlist` is a no-op ⇒ no pipeline is created.
#[tokio::test]
async fn ensure_registers_pipeline_for_runtime_active_playlist() {
    let (mut engine, pool) = engine_with_migrated_pool().await;
    // Engine boots with no active playlists → empty pipeline map.
    assert!(engine.pipelines.is_empty());

    // Runtime create: an active playlist row with a non-empty NDI name
    // (the observed 2026-08-06 ytyoung/id-515 case).
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (515, 'ytyoung', 'https://youtube', 'SP-young', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    engine.ensure_pipeline_for_playlist(515).await;

    assert!(
        engine.pipelines.contains_key(&515),
        "runtime-created active playlist must get a pipeline (#132)"
    );
}

/// #132 symmetric teardown: deleting/deactivating a playlist at runtime must
/// drop its pipeline (`PlaybackPipeline: Drop` → `Shutdown` → NDI sender
/// destroyed). RED: `remove_pipeline` is a no-op ⇒ the pipeline lingers.
#[tokio::test]
async fn remove_tears_down_runtime_pipeline() {
    let (mut engine, _pool) = engine_with_migrated_pool().await;
    engine.ensure_pipeline(515, "SP-young");
    assert!(engine.pipelines.contains_key(&515));

    engine.remove_pipeline(515);

    assert!(
        !engine.pipelines.contains_key(&515),
        "runtime-deleted playlist must have its pipeline torn down (#132)"
    );
}

/// The ensure gate mirrors the startup pre-create loop: an INACTIVE playlist
/// gets no pipeline. Kills a mutant that drops the `is_active` check.
#[tokio::test]
async fn ensure_skips_inactive_playlist() {
    let (mut engine, pool) = engine_with_migrated_pool().await;
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (7, 'p', 'u', 'SP-p', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();

    engine.ensure_pipeline_for_playlist(7).await;

    assert!(
        !engine.pipelines.contains_key(&7),
        "inactive playlist must NOT get a pipeline (#132)"
    );
}

/// An active playlist with an EMPTY ndi_output_name gets no pipeline — the same
/// gate the startup loop applies (`if !pl.ndi_output_name.is_empty()`). Kills a
/// mutant that drops the empty-name check.
#[tokio::test]
async fn ensure_skips_empty_ndi_name() {
    let (mut engine, pool) = engine_with_migrated_pool().await;
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (8, 'p', 'u', '', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    engine.ensure_pipeline_for_playlist(8).await;

    assert!(
        !engine.pipelines.contains_key(&8),
        "active playlist with empty NDI name must NOT get a pipeline (#132)"
    );
}

/// Ensuring a missing playlist row is a safe no-op (no panic, no pipeline).
#[tokio::test]
async fn ensure_noop_for_missing_row() {
    let (mut engine, _pool) = engine_with_migrated_pool().await;
    engine.ensure_pipeline_for_playlist(999).await;
    assert!(engine.pipelines.is_empty());
}

/// Ensure is idempotent: two calls create exactly one pipeline, so a re-sent
/// `EngineCommand::EnsurePipeline` is safe. RED: no-op stub creates zero.
#[tokio::test]
async fn ensure_is_idempotent() {
    let (mut engine, pool) = engine_with_migrated_pool().await;
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (9, 'p', 'u', 'SP-p', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    engine.ensure_pipeline_for_playlist(9).await;
    engine.ensure_pipeline_for_playlist(9).await;
    assert_eq!(
        engine.pipelines.len(),
        1,
        "ensure must be idempotent — exactly one pipeline for a repeated call (#132)"
    );
}

/// `remove_pipeline` on a playlist with no pipeline is a safe no-op.
#[tokio::test]
async fn remove_noop_when_absent() {
    let (mut engine, _pool) = engine_with_migrated_pool().await;
    engine.remove_pipeline(123); // must not panic
    assert!(engine.pipelines.is_empty());
}
