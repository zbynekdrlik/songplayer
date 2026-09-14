//! Mutation-kill tests for the `PlaybackEngine` genlock-wiring setters
//! (PR #153 / 0.47.0). Each setter's body was replaced with `()` by a
//! surviving mutant (a no-op), so each test asserts the setter actually
//! mutates the engine's observable state: the pacing flag flips, and the
//! injected clock-health / burn-registry handles are the SAME `Arc` the
//! setter was given (`Arc::ptr_eq`). Wired from `playback/mod.rs`, so this is
//! a child of `crate::playback` and can read the engine's private fields.

use super::clock_health::ClockHealth;
use super::ndi_burn::NdiBurnRegistry;
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
async fn set_genlock_pacing_flips_the_flag() {
    let mut engine = fresh_engine().await;
    assert!(
        !engine.genlock_pacing,
        "fresh engine defaults to pacing OFF"
    );
    engine.set_genlock_pacing(true);
    assert!(
        engine.genlock_pacing,
        "set_genlock_pacing(true) must set the flag (mutant no-op leaves it false)"
    );
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

#[tokio::test]
async fn set_ndi_burn_registry_injects_the_shared_registry() {
    let mut engine = fresh_engine().await;
    let registry = Arc::new(NdiBurnRegistry::new());
    engine.set_ndi_burn_registry(registry.clone());
    assert!(
        Arc::ptr_eq(&engine.ndi_burn_registry, &registry),
        "set_ndi_burn_registry must store the injected Arc (mutant no-op keeps the default)"
    );
}
