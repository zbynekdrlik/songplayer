//! The server's OBS wiring: [`start_obs`] (#219) is `lib::start`'s OBS
//! step — the OBS client (when configured), and what the rest of the server
//! needs from it.
//!
//! #221 L4b deleted the OBS → engine scene bridge (`run_obs_engine_bridge`,
//! `scene_change_commands`, `EngineCommand::SceneChanged`): SongPlayer's own
//! program drives playback (`playback::program_authority`). #221 L5 deleted
//! the OBS follow, and L6 the scene detection and transition reader it
//! read (`obs/mod.rs`).

use std::collections::HashMap;
use std::sync::Arc;

use sqlx::SqlitePool;
use tokio::sync::{RwLock, broadcast, mpsc};
use tracing::info;

use crate::obs;

/// What the server needs from its OBS side.
pub(crate) struct ObsWiring {
    /// cg OBS's events (the engine's, the #213 facade's).
    pub(crate) event_tx: broadcast::Sender<obs::ObsEvent>,
    /// The OBS client's command channel; `None` when OBS is not configured.
    pub(crate) cmd_tx: Option<mpsc::Sender<obs::ObsCommand>>,
    /// #196: the OBS-input → playlist-id map; `None` when OBS is not configured.
    pub(crate) ndi_sources: Option<obs::NdiSourceMap>,
}

/// Step 7 of `lib::start`: the OBS client, when OBS is configured.
#[cfg_attr(test, mutants::skip)] // startup wiring; the client is tested
pub(crate) async fn start_obs(
    pool: &SqlitePool,
    obs_state: &Arc<RwLock<obs::ObsState>>,
    rebuild_tx: &broadcast::Sender<()>,
    shutdown_tx: &broadcast::Sender<()>,
) -> Result<ObsWiring, sqlx::Error> {
    let (event_tx, _) = broadcast::channel::<obs::ObsEvent>(64);
    let Some(config) = obs::load_obs_config(pool).await? else {
        return Ok(ObsWiring {
            event_tx,
            cmd_tx: None,
            ndi_sources: None,
        });
    };
    let ndi_sources: obs::NdiSourceMap = Arc::new(RwLock::new(HashMap::new()));
    let client = obs::ObsClient::spawn(
        config,
        pool.clone(),
        ndi_sources.clone(),
        obs_state.clone(),
        event_tx.clone(),
        rebuild_tx.subscribe(),
        shutdown_tx.subscribe(),
    );
    info!("OBS WebSocket client started");
    Ok(ObsWiring {
        event_tx,
        cmd_tx: Some(client.cmd_sender()),
        ndi_sources: Some(ndi_sources),
    })
}
