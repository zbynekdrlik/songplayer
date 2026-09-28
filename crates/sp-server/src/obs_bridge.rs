//! OBS → engine scene bridge, and the server's OBS wiring.
//!
//! Translates OBS `SceneChanged` events into per-playlist
//! `EngineCommand::SceneChanged` messages for the playback engine.
//! [`start_obs`] (#219) is `lib::start`'s OBS step: the bridge, then the OBS
//! client (when configured), and what the rest of the server needs from them.

use std::collections::HashMap;
use std::sync::Arc;

use sqlx::SqlitePool;
use tokio::sync::{RwLock, broadcast, mpsc, watch};
use tracing::info;

use crate::EngineCommand;
use crate::obs;

/// What the server needs from its OBS side.
pub(crate) struct ObsWiring {
    /// cg OBS's events (the engine's, the #213 facade's).
    pub(crate) event_tx: broadcast::Sender<obs::ObsEvent>,
    /// The OBS client's command channel; `None` when OBS is not configured.
    pub(crate) cmd_tx: Option<mpsc::Sender<obs::ObsCommand>>,
    /// #196: the OBS-input → playlist-id map; `None` when OBS is not configured.
    pub(crate) ndi_sources: Option<obs::NdiSourceMap>,
    /// #219: the OBS client's published state (the program follow's input);
    /// a closed channel (disconnected, for good) when OBS is not configured.
    pub(crate) snapshots: watch::Receiver<obs::ObsSnapshot>,
}

/// Step 7 of `lib::start`: the OBS→engine bridge, then the OBS client.
///
/// The bridge subscribes to the event broadcast BEFORE the OBS client spawns.
/// On a fast LAN the client can connect, authenticate, rebuild the NDI source
/// map and broadcast the initial `SceneChanged` in under 50 ms — fast enough
/// to beat a subscription made after the spawn, and that initial scene
/// detection is what triggers auto-play on startup.
#[cfg_attr(test, mutants::skip)] // startup wiring; the bridge + client are tested
pub(crate) async fn start_obs(
    pool: &SqlitePool,
    obs_state: &Arc<RwLock<obs::ObsState>>,
    engine_tx: &mpsc::Sender<EngineCommand>,
    rebuild_tx: &broadcast::Sender<()>,
    shutdown_tx: &broadcast::Sender<()>,
) -> Result<ObsWiring, sqlx::Error> {
    let (event_tx, _) = broadcast::channel::<obs::ObsEvent>(64);
    tokio::spawn(run_obs_engine_bridge(
        event_tx.subscribe(),
        engine_tx.clone(),
        shutdown_tx.subscribe(),
    ));
    let Some(config) = obs::load_obs_config(pool).await? else {
        let (_, snapshots) = watch::channel(obs::ObsSnapshot::default());
        return Ok(ObsWiring {
            event_tx,
            cmd_tx: None,
            ndi_sources: None,
            snapshots,
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
        snapshots: client.snapshots(),
    })
}

/// Pure helper: compute the per-playlist engine commands that should
/// follow from an OBS `SceneChanged` event, given the previously-active
/// set.
///
/// For every playlist that was active before and is not active now,
/// emit `(pid, false)`. For every playlist that IS active now, emit
/// `(pid, true)` — **unconditionally**, even if it was already active
/// in the previous set. The `true` commands are idempotent at the
/// state machine level (`(Playing, SceneOn)` falls through to the
/// default no-op arm), so re-emitting them is safe.
///
/// Why unconditional on `true`: the engine state can be mutated
/// out-of-band — e.g. a REST `/pause` call transitions `Playing →
/// WaitingForScene` without the bridge seeing an OBS event. If the
/// bridge then naively diffed against its own tracked `previous` set,
/// a subsequent identical scene event (same scene, same active set)
/// would produce an empty diff and the engine would stay stuck in
/// `WaitingForScene` forever. Re-emitting `on_program: true` lets the
/// `(WaitingForScene, SceneOn) → SelectAndPlay` transition fire and
/// playback resumes. This behaviour is exercised by the
/// `bridge_re_emits_scene_on_after_external_state_change` test.
pub(crate) fn scene_change_commands(
    previous: &std::collections::HashSet<i64>,
    current: &std::collections::HashSet<i64>,
) -> Vec<(i64, bool)> {
    let mut out = Vec::new();

    // Playlists that just left the program scene.
    let mut newly_off: Vec<i64> = previous.difference(current).copied().collect();
    newly_off.sort_unstable();
    for pid in newly_off {
        out.push((pid, false));
    }

    // ALL currently-active playlists get `true` — idempotent at the
    // state machine level, but required so that a WaitingForScene
    // state (from an out-of-band pause) gets re-kicked.
    let mut all_on: Vec<i64> = current.iter().copied().collect();
    all_on.sort_unstable();
    for pid in all_on {
        out.push((pid, true));
    }

    out
}

/// Bridge task body — consumes `ObsEvent::SceneChanged` and
/// `ObsEvent::Disconnected` broadcasts and dispatches per-playlist
/// `EngineCommand::SceneChanged` messages to the playback engine.
pub(crate) async fn run_obs_engine_bridge(
    mut obs_event_rx: broadcast::Receiver<obs::ObsEvent>,
    engine_tx: mpsc::Sender<EngineCommand>,
    mut shutdown: broadcast::Receiver<()>,
) {
    use std::collections::HashSet;
    use tracing::debug;

    let mut previous: HashSet<i64> = HashSet::new();
    loop {
        tokio::select! {
            _ = shutdown.recv() => {
                debug!("OBS→engine scene bridge shutting down");
                break;
            }
            event = obs_event_rx.recv() => {
                let evt = match event {
                    Ok(e) => e,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("OBS→engine bridge lagged by {n} events");
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                match evt {
                    obs::ObsEvent::SceneChanged { active_playlist_ids, .. } => {
                        let cmds = scene_change_commands(&previous, &active_playlist_ids);
                        for (playlist_id, on_program) in cmds {
                            let _ = engine_tx
                                .send(EngineCommand::SceneChanged { playlist_id, on_program })
                                .await;
                        }
                        previous = active_playlist_ids;
                    }
                    obs::ObsEvent::Disconnected => {
                        // On disconnect, mark all previously-active playlists as off
                        // so the pipelines stop playback instead of continuing into
                        // the void.
                        for &pid in &previous {
                            let _ = engine_tx
                                .send(EngineCommand::SceneChanged {
                                    playlist_id: pid,
                                    on_program: false,
                                })
                                .await;
                        }
                        previous.clear();
                    }
                    obs::ObsEvent::Connected => {
                        // No-op: a fresh connect is always followed by a
                        // CurrentProgramSceneChanged event (either the initial
                        // GetCurrentProgramScene response or the next real
                        // scene switch), which will compute the correct active
                        // set and dispatch per-playlist SceneChanged commands
                        // from the `previous` diff above. Doing work here
                        // would race that event.
                    }
                    // #213: raw cg OBS events are for the remote-control facade;
                    // the engine reacts to the derived `SceneChanged` only.
                    obs::ObsEvent::Raw { .. } => {}
                }
            }
        }
    }
}
