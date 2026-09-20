//! Dashboard WebSocket handler — bidirectional message relay between
//! the UI and the server event bus.

use std::collections::HashMap;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures::{SinkExt, StreamExt};
use sqlx::Row;
use tracing::{debug, info, warn};

use sp_core::playback::{PlaybackMode, PlaybackState as WsPlaybackState};
use sp_core::ws::{ClientMsg, ServerMsg};

use crate::playback::ndi_health::{PipelineHealthSnapshot, PlaybackStateLabel};
use crate::{AppState, EngineCommand, SyncRequest};

/// Axum handler that upgrades an HTTP request to a WebSocket connection.
pub async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_ws(socket, state))
}

/// Bidirectional WebSocket relay.
///
/// - Forwards [`ServerMsg`] events from the broadcast channel to the client.
/// - Accepts [`ClientMsg`] from the client and dispatches to the engine.
async fn handle_ws(socket: WebSocket, state: AppState) {
    let (mut write, mut read) = socket.split();
    let mut event_rx = state.event_tx.subscribe();

    info!("WebSocket client connected");

    // Send initial state snapshot so the dashboard doesn't show stale data.
    {
        let obs = state.obs_state.read().await;
        let obs_status = ServerMsg::ObsStatus {
            connected: obs.connected,
            active_scene: obs.current_scene.clone(),
        };
        if let Ok(json) = serde_json::to_string(&obs_status) {
            let _ = write.send(Message::Text(json.into())).await;
        }
    }
    {
        let ts = state.tools_status.read().await;
        let tools_msg = ServerMsg::ToolsStatus {
            ytdlp_available: ts.ytdlp_available,
            ffmpeg_available: ts.ffmpeg_available,
            ytdlp_version: ts.ytdlp_version.clone(),
            js_runtime_ok: ts.js_runtime_ok,
            deno_version: ts.deno_version.clone(),
        };
        if let Ok(json) = serde_json::to_string(&tools_msg) {
            let _ = write.send(Message::Text(json.into())).await;
        }
    }
    // Replay the current per-playlist playback state so a dashboard opened
    // mid-song sees the playing card immediately, instead of Idle until the
    // next transition (#15 live preview + karaoke panel key off this state).
    {
        let modes: HashMap<i64, PlaybackMode> =
            match crate::db::models::get_active_playlists(&state.pool).await {
                Ok(playlists) => playlists
                    .into_iter()
                    .map(|p| (p.id, PlaybackMode::from_str_lossy(&p.playback_mode)))
                    .collect(),
                Err(e) => {
                    warn!("playback-state replay: failed to load playlist modes: {e}");
                    HashMap::new()
                }
            };
        let msgs = playback_state_replay(&state.ndi_health_registry.snapshots(), &modes);
        debug!(
            count = msgs.len(),
            "replaying playback state to new WS client"
        );
        for msg in &msgs {
            if let Ok(json) = serde_json::to_string(msg) {
                let _ = write.send(Message::Text(json.into())).await;
            }
        }
    }

    loop {
        tokio::select! {
            // Client -> Server
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientMsg>(&text) {
                            Ok(client_msg) => {
                                debug!(?client_msg, "received client message");
                                dispatch_client_msg(client_msg, &state).await;
                            }
                            Err(e) => {
                                warn!("invalid client message: {e}");
                                let err = ServerMsg::Error {
                                    message: format!("invalid message: {e}"),
                                };
                                if let Ok(json) = serde_json::to_string(&err) {
                                    let _ = write.send(Message::Text(json.into())).await;
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        info!("WebSocket client disconnected");
                        break;
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = write.send(Message::Pong(data)).await;
                    }
                    Some(Ok(_)) => {} // Binary, Pong — ignored
                    Some(Err(e)) => {
                        warn!("WebSocket read error: {e}");
                        break;
                    }
                }
            }

            // Server -> Client
            event = event_rx.recv() => {
                match event {
                    Ok(server_msg) => {
                        match serde_json::to_string(&server_msg) {
                            Ok(json) => {
                                if write.send(Message::Text(json.into())).await.is_err() {
                                    info!("WebSocket write failed, client disconnected");
                                    break;
                                }
                            }
                            Err(e) => {
                                warn!("failed to serialize server message: {e}");
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(n, "WebSocket client lagged, dropped messages");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        info!("event channel closed, closing WebSocket");
                        break;
                    }
                }
            }
        }
    }
}

/// Dispatch a parsed client message to the appropriate engine command.
async fn dispatch_client_msg(msg: ClientMsg, state: &AppState) {
    match msg {
        ClientMsg::Play { playlist_id } => {
            let _ = state
                .engine_tx
                .send(EngineCommand::Play { playlist_id })
                .await;
        }
        ClientMsg::Pause { playlist_id } => {
            let _ = state
                .engine_tx
                .send(EngineCommand::Pause { playlist_id })
                .await;
        }
        ClientMsg::Skip { playlist_id } => {
            let _ = state
                .engine_tx
                .send(EngineCommand::Skip { playlist_id })
                .await;
        }
        ClientMsg::Previous { playlist_id } => {
            // Previous is treated as skip for now (no previous track support yet).
            let _ = state
                .engine_tx
                .send(EngineCommand::Skip { playlist_id })
                .await;
        }
        ClientMsg::SetMode { playlist_id, mode } => {
            let _ = state
                .engine_tx
                .send(EngineCommand::SetMode { playlist_id, mode })
                .await;
        }
        ClientMsg::Seek {
            playlist_id,
            position_ms,
        } => {
            let _ = state
                .engine_tx
                .send(crate::EngineCommand::Seek {
                    playlist_id,
                    position_ms,
                })
                .await;
        }
        ClientMsg::SyncPlaylist { playlist_id } => {
            // Forward to the same sync_tx the REST `POST
            // /api/v1/playlists/{id}/sync` route uses (#139) — look up the
            // playlist's youtube_url the same way that route does.
            let row = sqlx::query("SELECT youtube_url FROM playlists WHERE id = ?")
                .bind(playlist_id)
                .fetch_optional(&state.pool)
                .await;
            match row {
                Ok(Some(row)) => {
                    let youtube_url: String = row.get("youtube_url");
                    info!(playlist_id, "sync playlist requested via WebSocket");
                    if let Err(e) = state
                        .sync_tx
                        .send(SyncRequest {
                            playlist_id,
                            youtube_url,
                        })
                        .await
                    {
                        warn!(playlist_id, "failed to queue sync via WebSocket: {e}");
                    }
                }
                Ok(None) => {
                    warn!(
                        playlist_id,
                        "sync playlist requested via WebSocket for unknown playlist id"
                    );
                }
                Err(e) => {
                    warn!(playlist_id, "sync playlist lookup failed: {e}");
                }
            }
        }
        ClientMsg::Ping => {
            // Pong is sent via the event channel — broadcast it.
            let _ = state.event_tx.send(ServerMsg::Pong);
        }
    }
}

/// Map an NDI-health snapshot's [`PlaybackStateLabel`] to the wire
/// [`WsPlaybackState`] used by [`ServerMsg::PlaybackStateChanged`]. The WS
/// protocol has no `Paused` variant, so a paused pipeline (including a
/// Playing-off-program pipeline, which `handle_health_snapshot` reconciles to
/// `Paused`) maps to the closest active-but-not-playing state,
/// `WaitingForScene`.
fn label_to_ws_state(label: &PlaybackStateLabel) -> WsPlaybackState {
    match label {
        PlaybackStateLabel::Idle => WsPlaybackState::Idle,
        PlaybackStateLabel::WaitingForScene => WsPlaybackState::WaitingForScene,
        PlaybackStateLabel::Playing => WsPlaybackState::Playing,
        PlaybackStateLabel::Paused => WsPlaybackState::WaitingForScene,
    }
}

/// Build the initial `PlaybackStateChanged` replay for a freshly connected
/// dashboard: one message per pipeline snapshot whose state is NOT `Idle`
/// (Idle is the dashboard default, so replaying it is pure noise). `modes` maps
/// `playlist_id` → configured [`PlaybackMode`]; a snapshot for a playlist not
/// in the map falls back to `PlaybackMode::default()`.
fn playback_state_replay(
    snapshots: &[PipelineHealthSnapshot],
    modes: &HashMap<i64, PlaybackMode>,
) -> Vec<ServerMsg> {
    snapshots
        .iter()
        .filter(|s| s.state != PlaybackStateLabel::Idle)
        .map(|s| ServerMsg::PlaybackStateChanged {
            playlist_id: s.playlist_id,
            state: label_to_ws_state(&s.state),
            mode: modes.get(&s.playlist_id).copied().unwrap_or_default(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(playlist_id: i64, state: PlaybackStateLabel) -> PipelineHealthSnapshot {
        use crate::playback::ndi_health::{AudioStats, PacingStats};
        PipelineHealthSnapshot {
            playlist_id,
            ndi_name: format!("SP-{playlist_id}"),
            state,
            connections: 1,
            frames_submitted_total: 0,
            frames_submitted_last_5s: 0,
            observed_fps: 24.0,
            nominal_fps: 24.0,
            last_submit_ts: None,
            last_heartbeat_ts: None,
            consecutive_bad_polls: 0,
            degraded_reason: None,
            clock: crate::playback::clock_health::ClockHealth::default(),
            pacing: PacingStats::default(),
            audio: AudioStats::default(),
            lock_state: sp_core::genlock::lock_state::LockState::Unlocked,
            lock_reason: String::new(),
            burn_on: false,
            recovery_step: None,
            sender_url: None,
        }
    }

    #[test]
    fn replay_maps_playing_and_skips_idle() {
        let snaps = vec![
            snapshot(1, PlaybackStateLabel::Playing),
            snapshot(2, PlaybackStateLabel::Idle),
        ];
        let mut modes = HashMap::new();
        modes.insert(1, PlaybackMode::Loop);
        let msgs = playback_state_replay(&snaps, &modes);
        // Idle (playlist 2) skipped — only playlist 1 replayed.
        assert_eq!(msgs.len(), 1);
        match &msgs[0] {
            ServerMsg::PlaybackStateChanged {
                playlist_id,
                state,
                mode,
            } => {
                assert_eq!(*playlist_id, 1);
                assert_eq!(*state, WsPlaybackState::Playing);
                assert_eq!(*mode, PlaybackMode::Loop);
            }
            other => panic!("expected PlaybackStateChanged, got {other:?}"),
        }
    }

    #[test]
    fn replay_unknown_playlist_uses_default_mode_and_paused_maps_to_waiting() {
        let snaps = vec![snapshot(7, PlaybackStateLabel::Paused)];
        // No mode entry for playlist 7 → default mode.
        let msgs = playback_state_replay(&snaps, &HashMap::new());
        assert_eq!(msgs.len(), 1);
        match &msgs[0] {
            ServerMsg::PlaybackStateChanged { state, mode, .. } => {
                assert_eq!(*state, WsPlaybackState::WaitingForScene);
                assert_eq!(*mode, PlaybackMode::default());
            }
            other => panic!("expected PlaybackStateChanged, got {other:?}"),
        }
    }

    #[test]
    fn client_msg_deserializes() {
        let json = r#"{"type":"Play","data":{"playlist_id":1}}"#;
        let msg: ClientMsg = serde_json::from_str(json).unwrap();
        assert_eq!(msg, ClientMsg::Play { playlist_id: 1 });
    }

    #[test]
    fn server_msg_serializes() {
        let msg = ServerMsg::Pong;
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("Pong"));
    }

    #[test]
    fn error_msg_serializes() {
        let msg = ServerMsg::Error {
            message: "test error".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("test error"));
    }
}
