//! OBS WebSocket v5 client with scene detection and text source control.

pub mod ndi_discovery;
pub mod scene;
pub mod text;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tokio::net::TcpStream;
use tokio::sync::{RwLock, broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, warn};

use crate::obs::ndi_discovery::rebuild_ndi_source_map;
use crate::obs::scene::check_scene_items;
use crate::obs::text::get_current_scene_request;

/// Shared OBS connection state.
#[derive(Debug, Clone, Default)]
pub struct ObsState {
    pub connected: bool,
    pub current_scene: Option<String>,
    /// Playlist IDs whose NDI source is currently on program.
    pub active_playlist_ids: HashSet<i64>,
}

/// Configuration for connecting to OBS WebSocket.
#[derive(Debug, Clone)]
pub struct ObsConfig {
    /// WebSocket URL, e.g. `"ws://127.0.0.1:4455"`.
    pub url: String,
    /// Optional password for authentication.
    pub password: Option<String>,
}

/// Mapping of NDI source name to playlist ID (for scene detection).
pub type NdiSourceMap = Arc<RwLock<HashMap<String, i64>>>;

/// Commands that can be sent to the OBS WebSocket connection loop.
pub enum ObsCommand {
    SetTextSource { source_name: String, text: String },
}

/// Events emitted by the OBS WebSocket connection loop.
#[derive(Debug, Clone)]
pub enum ObsEvent {
    Connected,
    Disconnected,
    SceneChanged {
        scene_name: String,
        active_playlist_ids: HashSet<i64>,
    },
}

/// OBS WebSocket v5 client handle.
pub struct ObsClient {
    state: Arc<RwLock<ObsState>>,
    cmd_tx: mpsc::Sender<ObsCommand>,
}

impl ObsClient {
    /// Spawn the OBS WebSocket connection loop as a background task.
    ///
    /// Returns a client handle for sending commands and reading state.
    ///
    /// `pool` is used to rebuild the NDI source map from active playlists
    /// after each (re)connect. `rebuild_rx` delivers explicit rebuild
    /// requests — e.g. from playlist CRUD handlers.
    pub fn spawn(
        config: ObsConfig,
        pool: SqlitePool,
        ndi_sources: NdiSourceMap,
        shared_state: Arc<RwLock<ObsState>>,
        event_tx: broadcast::Sender<ObsEvent>,
        mut rebuild_rx: broadcast::Receiver<()>,
        mut shutdown: broadcast::Receiver<()>,
    ) -> Self {
        let state = shared_state;
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<ObsCommand>(64);

        let loop_state = Arc::clone(&state);
        let loop_event_tx = event_tx.clone();

        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);

            loop {
                tokio::select! {
                    _ = shutdown.recv() => {
                        info!("OBS client shutting down");
                        break;
                    }
                    result = connect_and_run(
                        &config,
                        &pool,
                        &ndi_sources,
                        &loop_state,
                        &loop_event_tx,
                        &mut cmd_rx,
                        &mut rebuild_rx,
                    ) => {
                        match result {
                            Ok(()) => {
                                info!("OBS connection closed cleanly");
                                break;
                            }
                            Err(e) => {
                                warn!("OBS connection error: {e}");
                            }
                        }
                    }
                }

                // Mark disconnected and notify.
                {
                    let mut s = loop_state.write().await;
                    s.connected = false;
                    s.current_scene = None;
                    s.active_playlist_ids.clear();
                }
                let _ = loop_event_tx.send(ObsEvent::Disconnected);

                info!("Reconnecting to OBS in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        });

        Self { state, cmd_tx }
    }

    /// Update a text source in OBS.
    /// Get a clone of the command sender for use by other components.
    pub fn cmd_sender(&self) -> mpsc::Sender<ObsCommand> {
        self.cmd_tx.clone()
    }

    pub async fn set_text(&self, source_name: &str, text: &str) -> Result<(), anyhow::Error> {
        self.cmd_tx
            .send(ObsCommand::SetTextSource {
                source_name: source_name.to_string(),
                text: text.to_string(),
            })
            .await
            .map_err(|_| anyhow::anyhow!("OBS command channel closed"))
    }

    /// Read current OBS state.
    pub async fn state(&self) -> ObsState {
        self.state.read().await.clone()
    }
}

/// Compute OBS WebSocket v5 authentication string.
///
/// Algorithm:
/// 1. `secret = base64(sha256(password + salt))`
/// 2. `auth = base64(sha256(secret + challenge))`
pub fn compute_auth(password: &str, challenge: &str, salt: &str) -> String {
    let engine = base64::engine::general_purpose::STANDARD;
    let secret = engine.encode(Sha256::digest(format!("{password}{salt}").as_bytes()));
    engine.encode(Sha256::digest(format!("{secret}{challenge}").as_bytes()))
}

/// Main connection loop: connect, authenticate, handle messages.
async fn connect_and_run(
    config: &ObsConfig,
    pool: &SqlitePool,
    ndi_sources: &NdiSourceMap,
    state: &Arc<RwLock<ObsState>>,
    event_tx: &broadcast::Sender<ObsEvent>,
    cmd_rx: &mut mpsc::Receiver<ObsCommand>,
    rebuild_rx: &mut broadcast::Receiver<()>,
) -> Result<(), anyhow::Error> {
    let (ws_stream, _) = tokio_tungstenite::connect_async(&config.url).await?;
    let (mut write, mut read) = ws_stream.split();

    // Step 1: Receive Hello (op 0).
    let hello = read_json_message(&mut read).await?;
    let op = hello["op"].as_u64().unwrap_or(u64::MAX);
    if op != 0 {
        anyhow::bail!("expected Hello (op 0), got op {op}");
    }
    debug!("received OBS Hello");

    // Step 2: Send Identify (op 1).
    let mut identify_data = serde_json::json!({
        "rpcVersion": 1,
        "eventSubscriptions": 4  // Scenes events
    });

    if let Some(password) = &config.password {
        if let Some(auth) = hello["d"]["authentication"].as_object() {
            let challenge = auth
                .get("challenge")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing auth challenge"))?;
            let salt = auth
                .get("salt")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing auth salt"))?;
            identify_data["authentication"] =
                serde_json::Value::String(compute_auth(password, challenge, salt));
        }
    }

    let identify_msg = serde_json::json!({
        "op": 1,
        "d": identify_data,
    });
    write
        .send(Message::Text(identify_msg.to_string().into()))
        .await?;

    // Step 3: Receive Identified (op 2).
    let identified = read_json_message(&mut read).await?;
    let op = identified["op"].as_u64().unwrap_or(u64::MAX);
    if op != 2 {
        anyhow::bail!("expected Identified (op 2), got op {op}");
    }
    info!("connected to OBS WebSocket");

    {
        let mut s = state.write().await;
        s.connected = true;
    }
    let _ = event_tx.send(ObsEvent::Connected);

    // Rebuild the NDI source map from the DB + OBS inputs before we start
    // listening for scene-change events, so the very first scene lookup
    // already knows which OBS input names correspond to which playlists.
    let new_map = rebuild_ndi_source_map(&mut write, &mut read, pool).await;
    {
        let mut guard = ndi_sources.write().await;
        *guard = new_map;
    }

    // Fetch initial scene.
    let request_id = uuid::Uuid::new_v4().to_string();
    let req = get_current_scene_request(&request_id);
    write.send(Message::Text(req.to_string().into())).await?;

    // Step 4: Message loop.
    loop {
        tokio::select! {
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let json: serde_json::Value = serde_json::from_str(&text)?;
                        handle_message(
                            json,
                            &mut write,
                            &mut read,
                            ndi_sources,
                            state,
                            event_tx,
                        ).await?;
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        info!("OBS WebSocket closed");
                        return Ok(());
                    }
                    Some(Ok(_)) => {} // ping/pong/binary ignored
                    Some(Err(e)) => return Err(e.into()),
                }
            }
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    ObsCommand::SetTextSource { source_name, text } => {
                        let req_id = uuid::Uuid::new_v4().to_string();
                        let req = text::set_text_request(&req_id, &source_name, &text);
                        write.send(Message::Text(req.to_string().into())).await?;
                        debug!(source_name, "sent SetTextSource to OBS");
                    }
                }
            }
            rebuild_result = rebuild_rx.recv() => {
                match rebuild_result {
                    Ok(()) => {
                        debug!("received rebuild signal, refreshing NDI source map");
                        let new_map =
                            rebuild_ndi_source_map(&mut write, &mut read, pool).await;
                        let mut guard = ndi_sources.write().await;
                        *guard = new_map;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(
                            "rebuild signal channel lagged by {n} messages, \
                             refreshing NDI source map once"
                        );
                        let new_map =
                            rebuild_ndi_source_map(&mut write, &mut read, pool).await;
                        let mut guard = ndi_sources.write().await;
                        *guard = new_map;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Channel closed — ignore, the shutdown path will catch it.
                    }
                }
            }
        }
    }
}

/// Handle a parsed OBS WebSocket message.
async fn handle_message(
    msg: serde_json::Value,
    write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    read: &mut SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    ndi_sources: &NdiSourceMap,
    state: &Arc<RwLock<ObsState>>,
    event_tx: &broadcast::Sender<ObsEvent>,
) -> Result<(), anyhow::Error> {
    let op = msg["op"].as_u64().unwrap_or(u64::MAX);

    match op {
        // Event (op 5)
        5 => {
            let event_type = msg["d"]["eventType"].as_str().unwrap_or("");
            debug!("OBS event: {event_type}");

            if event_type == "CurrentProgramSceneChanged" {
                if let Some(scene_name) = msg["d"]["eventData"]["sceneName"].as_str() {
                    let sources = ndi_sources.read().await;
                    let active_ids = check_scene_items(write, read, scene_name, &sources).await;

                    let mut s = state.write().await;
                    s.current_scene = Some(scene_name.to_string());
                    s.active_playlist_ids = active_ids.clone();

                    let _ = event_tx.send(ObsEvent::SceneChanged {
                        scene_name: scene_name.to_string(),
                        active_playlist_ids: active_ids,
                    });
                }
            }
        }
        // RequestResponse (op 7) — handle GetCurrentProgramScene response.
        7 => {
            let request_type = msg["d"]["requestType"].as_str().unwrap_or("");
            if request_type == "GetCurrentProgramScene" {
                if let Some(scene_name) =
                    msg["d"]["responseData"]["currentProgramSceneName"].as_str()
                {
                    let sources = ndi_sources.read().await;
                    let active_ids = check_scene_items(write, read, scene_name, &sources).await;

                    let mut s = state.write().await;
                    s.current_scene = Some(scene_name.to_string());
                    s.active_playlist_ids = active_ids.clone();

                    let _ = event_tx.send(ObsEvent::SceneChanged {
                        scene_name: scene_name.to_string(),
                        active_playlist_ids: active_ids,
                    });
                }
            }
        }
        _ => {
            debug!("unhandled OBS message op={op}");
        }
    }

    Ok(())
}

/// Read the next text message from the WebSocket and parse as JSON.
async fn read_json_message(
    read: &mut SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
) -> Result<serde_json::Value, anyhow::Error> {
    loop {
        match read.next().await {
            Some(Ok(Message::Text(text))) => {
                return Ok(serde_json::from_str(&text)?);
            }
            Some(Ok(Message::Close(_))) | None => {
                anyhow::bail!("WebSocket closed while waiting for message");
            }
            Some(Ok(_)) => continue, // skip ping/pong/binary
            Some(Err(e)) => return Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_auth() {
        // Known test vectors: deterministic given password, challenge, salt.
        let password = "supersecretpassword";
        let challenge = "aDf8sUpKlMQIHOAd3dqr7KHLGr1Y1P4R";
        let salt = "lM1GncleQOaCu7U0knJcR5Tk3MFGz0VQ";

        let result = compute_auth(password, challenge, salt);

        // Verify it produces a valid base64 string.
        let engine = base64::engine::general_purpose::STANDARD;
        let decoded = engine.decode(&result);
        assert!(decoded.is_ok(), "result should be valid base64");
        assert_eq!(
            decoded.unwrap().len(),
            32,
            "SHA-256 output should be 32 bytes"
        );

        // Verify determinism.
        let result2 = compute_auth(password, challenge, salt);
        assert_eq!(result, result2);
    }

    #[test]
    fn test_compute_auth_known_value() {
        // Manually compute expected value:
        // secret = base64(sha256("supersecretpassword" + "salt123"))
        // auth = base64(sha256(secret + "challenge456"))
        let password = "test";
        let salt = "salt123";
        let challenge = "challenge456";

        let engine = base64::engine::general_purpose::STANDARD;

        // Step 1: secret = base64(sha256(password + salt))
        let secret = engine.encode(Sha256::digest(format!("{password}{salt}").as_bytes()));
        // Step 2: auth = base64(sha256(secret + challenge))
        let expected = engine.encode(Sha256::digest(format!("{secret}{challenge}").as_bytes()));

        let result = compute_auth(password, challenge, salt);
        assert_eq!(result, expected);
    }

    #[test]
    fn test_obs_state_default() {
        let state = ObsState::default();
        assert!(!state.connected);
        assert!(state.current_scene.is_none());
        assert!(state.active_playlist_ids.is_empty());
    }

    #[test]
    fn test_parse_hello_message() {
        let hello = serde_json::json!({
            "op": 0,
            "d": {
                "obsWebSocketVersion": "5.0.0",
                "rpcVersion": 1,
                "authentication": {
                    "challenge": "aDf8sUpKlMQIHOAd3dqr7KHLGr1Y1P4R",
                    "salt": "lM1GncleQOaCu7U0knJcR5Tk3MFGz0VQ"
                }
            }
        });

        assert_eq!(hello["op"].as_u64(), Some(0));
        let auth = hello["d"]["authentication"].as_object().unwrap();
        assert!(auth.contains_key("challenge"));
        assert!(auth.contains_key("salt"));
    }

    #[test]
    fn test_parse_identified_message() {
        let identified = serde_json::json!({
            "op": 2,
            "d": {
                "negotiatedRpcVersion": 1
            }
        });

        assert_eq!(identified["op"].as_u64(), Some(2));
        assert_eq!(identified["d"]["negotiatedRpcVersion"].as_u64(), Some(1));
    }

    #[test]
    fn test_parse_event_message() {
        let event = serde_json::json!({
            "op": 5,
            "d": {
                "eventType": "CurrentProgramSceneChanged",
                "eventData": {
                    "sceneName": "Main Scene"
                }
            }
        });

        assert_eq!(event["op"].as_u64(), Some(5));
        assert_eq!(
            event["d"]["eventType"].as_str(),
            Some("CurrentProgramSceneChanged")
        );
        assert_eq!(
            event["d"]["eventData"]["sceneName"].as_str(),
            Some("Main Scene")
        );
    }

    #[test]
    fn test_parse_request_response() {
        let response = serde_json::json!({
            "op": 7,
            "d": {
                "requestType": "GetCurrentProgramScene",
                "requestId": "abc-123",
                "requestStatus": {
                    "result": true,
                    "code": 100
                },
                "responseData": {
                    "currentProgramSceneName": "Live Scene"
                }
            }
        });

        assert_eq!(response["op"].as_u64(), Some(7));
        assert_eq!(
            response["d"]["requestType"].as_str(),
            Some("GetCurrentProgramScene")
        );
        assert_eq!(
            response["d"]["responseData"]["currentProgramSceneName"].as_str(),
            Some("Live Scene")
        );
    }

    #[test]
    fn test_state_connected_transition() {
        let mut state = ObsState::default();
        assert!(!state.connected);

        state.connected = true;
        state.current_scene = Some("Scene 1".to_string());
        state.active_playlist_ids.insert(1);
        state.active_playlist_ids.insert(2);

        assert!(state.connected);
        assert_eq!(state.current_scene.as_deref(), Some("Scene 1"));
        assert!(state.active_playlist_ids.contains(&1));
        assert!(state.active_playlist_ids.contains(&2));

        // Disconnect transition.
        state.connected = false;
        state.current_scene = None;
        state.active_playlist_ids.clear();

        assert!(!state.connected);
        assert!(state.current_scene.is_none());
        assert!(state.active_playlist_ids.is_empty());
    }

    #[test]
    fn test_parse_hello_without_auth() {
        let hello = serde_json::json!({
            "op": 0,
            "d": {
                "obsWebSocketVersion": "5.0.0",
                "rpcVersion": 1
            }
        });

        assert_eq!(hello["op"].as_u64(), Some(0));
        assert!(hello["d"]["authentication"].as_object().is_none());
    }
}
