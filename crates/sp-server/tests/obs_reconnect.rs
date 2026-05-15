//! Integration test for #80: `ObsClient` must reconnect on every
//! disconnect — including a clean server-side WebSocket close — not
//! only on transport errors.
//!
//! The 2026-05-03 production failure was a clean close that returned
//! `Ok(())` from `connect_and_run`, hitting the `break` in the
//! reconnect supervisor and leaving SongPlayer permanently OBS-deaf
//! until process restart.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{FakeObsServer, FakeObsState};
use sp_server::db;
use sp_server::obs;
use tokio::sync::{RwLock, broadcast};

#[tokio::test]
async fn obs_client_reconnects_after_server_initiated_clean_close() {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();

    // FakeObsServer sends a Close frame right after Identified. The
    // tungstenite read side returns `Ok(None)` next, the OBS client's
    // event loop falls out of `read.next()`, and `connect_and_run`
    // returns `Ok(())`. Under the bug the reconnect supervisor breaks
    // out of its loop on that path → no reconnect.
    let mut fake_state = FakeObsState::default();
    fake_state.close_after_identify = true;
    let fake_obs = FakeObsServer::spawn_with_state(fake_state).await;

    let ndi_sources: obs::NdiSourceMap = Arc::new(RwLock::new(HashMap::new()));
    let obs_state = Arc::new(RwLock::new(obs::ObsState::default()));
    let (obs_event_tx, _obs_event_rx) = broadcast::channel::<obs::ObsEvent>(16);
    let (_obs_rebuild_tx, obs_rebuild_rx) = broadcast::channel::<()>(4);
    let (_shutdown_tx, shutdown_rx) = broadcast::channel::<()>(1);

    let _client = obs::ObsClient::spawn(
        obs::ObsConfig {
            url: fake_obs.url(),
            password: None,
        },
        pool.clone(),
        ndi_sources,
        obs_state,
        obs_event_tx,
        obs_rebuild_rx,
        shutdown_rx,
    );

    // Reconnect backoff in the OBS client starts at 1 s and caps at 5 s,
    // so a healthy client must complete >= 2 connections inside ~10 s.
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let count = fake_obs.accept_count();
        if count >= 2 {
            break;
        }
        if Instant::now() > deadline {
            panic!(
                "ObsClient did not reconnect after server-side clean close: \
                 accept_count={count} after 12s (expected >= 2)"
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    fake_obs.shutdown().await;
}
