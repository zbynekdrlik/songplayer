//! Dashboard WebSocket handler — bidirectional message relay between
//! the UI and the server event bus.

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures::stream::SplitSink;
use futures::{Sink, SinkExt, StreamExt};
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
    send_status(&mut write, &state).await;
    // Replay every playlist's playback state and a playing one's song so a
    // dashboard opened mid-song shows what plays at once, instead of Idle or
    // "Nič nehrá" until the next broadcast (#15 live preview + karaoke panel
    // key off this state; #225 the song + an explicit Idle per playlist).
    {
        let count = send_replay(&mut write, &state).await;
        debug!(count, "replaying playback state to new WS client");
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
                        // #225: a dropped state change would leave the client on
                        // a stale state until the next one, so re-tell the truth.
                        // Review round 4: resubscribe FIRST (the buffered tail is
                        // older than the replay and would roll the client back),
                        // then the replay, as on connect.
                        warn!(n, "WebSocket client lagged, dropped messages; re-sending the replay");
                        event_rx = event_rx.resubscribe();
                        send_replay(&mut write, &state).await;
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
            // #225 unit 2: the row first, then the engine, the same path as
            // the REST mode route (`routes_mode`); a mode not saved is told
            // to every open dashboard (the error banner, in Slovak).
            let saved = super::routes_mode::persist_then_tell(
                &state.pool,
                &state.engine_tx,
                playlist_id,
                mode,
            )
            .await;
            if !matches!(saved, Ok(true)) {
                let _ = state.event_tx.send(ServerMsg::Error {
                    message: format!("Režim prehrávania playlistu {playlist_id} sa neuložil"),
                });
            }
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

/// Send a newly connected dashboard the OBS status, then the tools status.
async fn send_status<S>(write: &mut S, state: &AppState)
where
    S: Sink<Message> + Unpin,
{
    {
        let obs = state.obs_state.read().await;
        let obs_status = ServerMsg::ObsStatus {
            connected: obs.connected,
            active_scene: obs.current_scene.clone(),
        };
        send_json(write, &obs_status).await;
    }
    {
        let ts = state.tools_status.read().await;
        send_json(write, &ts.message()).await;
    }
}

/// One message as a JSON text frame. A failed send is not handled here: the
/// read loop sees the closed socket.
async fn send_json<S>(write: &mut S, msg: &ServerMsg)
where
    S: Sink<Message> + Unpin,
{
    if let Ok(json) = serde_json::to_string(msg) {
        let _ = write.send(Message::Text(json.into())).await;
    }
}

/// Send [`on_connect_replay`] to one client; returns how many messages.
async fn send_replay(write: &mut SplitSink<WebSocket, Message>, state: &AppState) -> usize {
    let msgs = on_connect_replay(state).await;
    for msg in &msgs {
        if let Ok(json) = serde_json::to_string(msg) {
            let _ = write.send(Message::Text(json.into())).await;
        }
    }
    msgs.len()
}

/// The messages a newly connected dashboard is sent first, after the OBS and
/// tools status (#225): for EVERY playlist in the DB, the engine's last
/// dashboard message about it (`playback/dashboard_replay.rs`): a playing
/// one's song, then its state; any other an explicit `Idle`; each in the mode
/// the engine plays (one it has not told about: its row's mode, the one its
/// pipeline starts in, #225 unit 2). So the Player knows from the first
/// batch what plays and what does not. `handle_ws` subscribes to the event
/// bus before it calls this, so nothing the engine sends in between is lost.
/// A failed DB read still replays every playlist the engine has told the
/// dashboard about.
pub(crate) async fn on_connect_replay(state: &AppState) -> Vec<ServerMsg> {
    let playlists = match crate::db::models_playlists::all_playlist_modes(&state.pool).await {
        Ok(playlists) => playlists,
        Err(e) => {
            warn!("on-connect replay: failed to load the playlists: {e}");
            Vec::new()
        }
    };
    crate::playback::dashboard_replay::global().replay(&playlists)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::convert::Infallible;
    use std::sync::Arc;

    use tokio::sync::{Semaphore, mpsc};

    /// #144: a new dashboard's first frames (the OBS status, then the tools
    /// status) are sent with no status lock held. The client here takes a
    /// frame only when the test lets it, like a slow one with a full socket
    /// buffer. While a frame waits, both locks must be free: a tokio `RwLock`
    /// queues every reader behind a waiting writer, so a held read guard
    /// would stall the OBS client's writes and every status read behind them.
    #[tokio::test]
    async fn a_slow_client_holds_no_status_lock_while_it_takes_the_status() {
        let state = crate::api::routes::tests::test_state().await;
        {
            let mut obs = state.obs_state.write().await;
            obs.connected = true;
            obs.current_scene = Some("sp-fast".to_string());
        }
        {
            let mut tools = state.tools_status.write().await;
            tools.ytdlp_available = true;
            tools.ytdlp_version = Some("2026.09.30".to_string());
        }
        let (seen_tx, mut seen_rx) = mpsc::unbounded_channel::<Message>();
        let release = Arc::new(Semaphore::new(0));
        let gate = Arc::clone(&release);
        let client = futures::sink::unfold((), move |(), frame: Message| {
            let seen_tx = seen_tx.clone();
            let gate = Arc::clone(&gate);
            async move {
                seen_tx.send(frame).expect("the test reads every frame");
                gate.acquire().await.expect("the gate stays open").forget();
                Ok::<(), Infallible>(())
            }
        });
        let sender_state = state.clone();
        let sender = tokio::spawn(async move {
            let mut client = Box::pin(client);
            send_status(&mut client, &sender_state).await;
        });

        let mut taken = Vec::new();
        for _ in 0..2 {
            let frame = seen_rx.recv().await.expect("a status frame");
            assert!(
                state.obs_state.try_write().is_ok(),
                "the OBS status is locked while a frame waits for the client"
            );
            assert!(
                state.tools_status.try_write().is_ok(),
                "the tools status is locked while a frame waits for the client"
            );
            let text = match frame {
                Message::Text(text) => text,
                other => panic!("expected a text frame, got {other:?}"),
            };
            taken.push(serde_json::from_str::<ServerMsg>(text.as_str()).unwrap());
            release.add_permits(1);
        }
        sender.await.unwrap();
        assert_eq!(
            taken,
            vec![
                ServerMsg::ObsStatus {
                    connected: true,
                    active_scene: Some("sp-fast".to_string()),
                },
                ServerMsg::ToolsStatus {
                    ytdlp_available: true,
                    ffmpeg_available: false,
                    ytdlp_version: Some("2026.09.30".to_string()),
                    js_runtime_ok: false,
                    deno_version: None,
                },
            ]
        );
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

    /// #225 unit 2: the WS `SetMode` takes the REST mode route's path — the
    /// playlist's row first, then the engine; a mode not saved is told back.
    #[tokio::test]
    async fn a_ws_set_mode_saves_the_row_then_tells_the_engine() {
        use sp_core::playback::PlaybackMode;

        let (state, mut engine_rx) = crate::api::routes::tests::test_state_with_engine_rx().await;
        sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (22538, 'ws', 'u-ws')")
            .execute(&state.pool)
            .await
            .unwrap();
        let mut told = state.event_tx.subscribe();

        let set_mode = |playlist_id| ClientMsg::SetMode {
            playlist_id,
            mode: PlaybackMode::Loop,
        };
        dispatch_client_msg(set_mode(22_538), &state).await;

        let stored: String =
            sqlx::query_scalar("SELECT playback_mode FROM playlists WHERE id = 22538")
                .fetch_one(&state.pool)
                .await
                .unwrap();
        assert_eq!(stored, "loop", "the row holds the mode");
        assert!(matches!(
            engine_rx.try_recv(),
            Ok(EngineCommand::SetMode {
                playlist_id: 22_538,
                mode: PlaybackMode::Loop
            })
        ));
        assert!(told.try_recv().is_err(), "a saved mode is no error");

        // No such playlist: nothing saved, the engine not told, the client is.
        dispatch_client_msg(set_mode(22_539), &state).await;
        assert!(engine_rx.try_recv().is_err());
        // In Slovak, like every text the dashboard shows (review round 1).
        match told.try_recv() {
            Ok(ServerMsg::Error { message }) => {
                assert!(message.contains("sa neuložil"), "{message}");
            }
            other => panic!("expected the not-saved Error, got {other:?}"),
        }
    }
}
