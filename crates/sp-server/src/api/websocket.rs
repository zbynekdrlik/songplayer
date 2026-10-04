//! Dashboard WebSocket handler — bidirectional message relay between
//! the UI and the server event bus.

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures::{SinkExt, StreamExt};
use sqlx::Row;
use tracing::{debug, info, warn};

use sp_core::ws::{ClientMsg, ServerMsg};

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
    // Replay every playlist's playback state and a playing one's song so a
    // dashboard opened mid-song shows what plays at once, instead of Idle or
    // "Nič nehrá" until the next broadcast (#15 live preview + karaoke panel
    // key off this state; #225 the song + an explicit Idle per playlist).
    {
        let msgs = on_connect_replay(&state).await;
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

/// The messages a newly connected dashboard is sent first, after the OBS and
/// tools status (#225): for EVERY playlist in the DB, the engine's last
/// dashboard message about it (`playback/dashboard_replay.rs`): a playing
/// one's song, then its state; any other an explicit `Idle`; each in the mode
/// the engine plays. So the Player knows from the first batch what plays and
/// what does not. `handle_ws` subscribes to the event bus before it calls
/// this, so nothing the engine sends in between is lost. A failed DB read
/// still replays every playlist the engine has told the dashboard about.
pub(crate) async fn on_connect_replay(state: &AppState) -> Vec<ServerMsg> {
    let ids = match crate::db::models_playlists::all_playlist_ids(&state.pool).await {
        Ok(ids) => ids,
        Err(e) => {
            warn!("on-connect replay: failed to load the playlists: {e}");
            Vec::new()
        }
    };
    crate::playback::dashboard_replay::global().replay(&ids)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
