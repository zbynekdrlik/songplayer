//! OBS → engine scene bridge.
//!
//! Translates OBS `SceneChanged` events into per-playlist
//! `EngineCommand::SceneChanged` messages for the playback engine.

use tokio::sync::{broadcast, mpsc};

use crate::EngineCommand;
use crate::obs;

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
                }
            }
        }
    }
}
