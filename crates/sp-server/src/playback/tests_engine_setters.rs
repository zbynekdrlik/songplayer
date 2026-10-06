//! Mutation-kill tests for the `PlaybackEngine` genlock-wiring setters
//! (PR #153 / 0.47.0). A setter's body replaced with `()` by a surviving
//! mutant (a no-op) is caught by asserting the injected clock-health handle
//! is the SAME `Arc` the setter was given (`Arc::ptr_eq`). #221 lane 3
//! deleted the pacing flag (pacing is the only path) and the burn registry
//! (it painted only the per-playlist senders) with their setters. Wired from
//! `playback/mod.rs`, so this is a child of `crate::playback` and can read
//! the engine's private fields.

use super::clock_health::ClockHealth;
use super::ndi_health::NdiHealthRegistry;
use super::{PlaybackEngine, PlaybackEngineConfig};
use sp_core::ws::ServerMsg;
use sqlx::SqlitePool;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use tokio::sync::{broadcast, mpsc};

async fn fresh_engine() -> PlaybackEngine {
    let pool = SqlitePool::connect(":memory:").await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (ws_tx, _) = broadcast::channel::<ServerMsg>(16);
    PlaybackEngine::new(PlaybackEngineConfig {
        pool,
        cache_dir: PathBuf::from("/tmp"),
        obs_event_tx: obs_tx,
        obs_cmd_tx: None,
        resolume_tx,
        ws_event_tx: ws_tx,
        presenter_client: None,
        ndi_health_registry: Arc::new(NdiHealthRegistry::new()),
    })
}

#[tokio::test]
async fn set_clock_health_injects_the_shared_handle() {
    let mut engine = fresh_engine().await;
    let handle = Arc::new(RwLock::new(ClockHealth::default()));
    engine.set_clock_health(handle.clone());
    assert!(
        Arc::ptr_eq(&engine.clock_health, &handle),
        "set_clock_health must store the injected Arc (mutant no-op keeps the default)"
    );
}
