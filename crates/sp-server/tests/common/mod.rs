//! Shared test harness — `FakeObsServer` that speaks enough of the OBS
//! WebSocket 5.x protocol to drive sp-server's OBS client in integration
//! tests without a real OBS process.
//!
//! Covers:
//! - Hello (op 0) → Identify (op 1) → Identified (op 2) handshake with no auth.
//! - RequestResponse (op 7) replies to `GetInputList`, `GetInputSettings`,
//!   `GetSceneItemList`.
//! - Pushing `CurrentProgramSceneChanged` (op 5 / eventType) events via a
//!   control channel.
//!
//! Rust convention: files under `tests/common/` are automatically excluded
//! from the integration-test binary list, so each test file can `mod common;`
//! without spawning a dead binary.

#![allow(dead_code)] // Not every helper is used by every test file.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

/// Scripted state the fake OBS reveals to its clients.
#[derive(Clone, Default)]
pub struct FakeObsState {
    /// Map of OBS input name → inputKind (e.g. `"sp-fast_video"` → `"ndi_source"`).
    pub inputs: HashMap<String, String>,
    /// Map of OBS input name → an `inputSettings` JSON object (for NDI inputs,
    /// this typically contains an `ndi_source_name` field).
    pub input_settings: HashMap<String, Value>,
    /// Map of scene name → list of scene items, each tuple is
    /// `(sourceName, isGroup, inputKind)`.
    pub scene_items: HashMap<String, Vec<(String, bool, String)>>,
    /// When true, the fake server silently drops `GetInputList` requests —
    /// no response at all. Simulates the transient WebSocket failure that
    /// broke scene detection on 2026-04-19 (production OBS returned
    /// nothing for GetInputList; the old code wiped the NDI source map).
    pub suppress_get_input_list: bool,
}

/// A fake OBS WebSocket server listening on a random localhost port.
pub struct FakeObsServer {
    addr: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    event_tx: mpsc::Sender<Value>,
    state: Arc<Mutex<FakeObsState>>,
}

impl FakeObsServer {
    /// Spawn a server with an empty state.
    pub async fn spawn() -> Self {
        Self::spawn_with_state(FakeObsState::default()).await
    }

    /// Spawn a server pre-seeded with the given state.
    pub async fn spawn_with_state(initial: FakeObsState) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind localhost:0");
        let addr = listener.local_addr().expect("local_addr");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (event_tx, event_rx) = mpsc::channel::<Value>(32);
        let state = Arc::new(Mutex::new(initial));
        let state_clone = state.clone();

        tokio::spawn(async move {
            run_accept_loop(listener, shutdown_rx, event_rx, state_clone).await;
        });

        Self {
            addr,
            shutdown_tx: Some(shutdown_tx),
            event_tx,
            state,
        }
    }

    /// WebSocket URL clients should connect to.
    pub fn url(&self) -> String {
        format!("ws://{}", self.addr)
    }

    /// Push a `CurrentProgramSceneChanged` event to the currently connected client.
    pub async fn push_program_scene_change(&self, scene_name: &str) {
        let evt = json!({
            "op": 5,
            "d": {
                "eventType": "CurrentProgramSceneChanged",
                "eventIntent": 0,
                "eventData": { "sceneName": scene_name }
            }
        });
        let _ = self.event_tx.send(evt).await;
    }

    /// Mutate the fake state (e.g. to simulate a new NDI input appearing).
    pub async fn update_state<F>(&self, f: F)
    where
        F: FnOnce(&mut FakeObsState),
    {
        let mut s = self.state.lock().await;
        f(&mut s);
    }

    /// Shut down the accept loop.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

async fn run_accept_loop(
    listener: TcpListener,
    mut shutdown_rx: oneshot::Receiver<()>,
    event_rx: mpsc::Receiver<Value>,
    state: Arc<Mutex<FakeObsState>>,
) {
    // Wrap event_rx in a Mutex so `handle_client` can borrow it when a client connects.
    let event_rx = Arc::new(Mutex::new(event_rx));

    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((tcp, _)) => {
                        let ws = match tokio_tungstenite::accept_async(tcp).await {
                            Ok(s) => s,
                            Err(_) => continue,
                        };
                        let state_clone = state.clone();
                        let event_rx_clone = event_rx.clone();
                        tokio::spawn(async move {
                            handle_client(ws, event_rx_clone, state_clone).await;
                        });
                    }
                    Err(_) => return,
                }
            }
            _ = &mut shutdown_rx => return,
        }
    }
}

async fn handle_client(
    ws: WebSocketStream<tokio::net::TcpStream>,
    event_rx: Arc<Mutex<mpsc::Receiver<Value>>>,
    state: Arc<Mutex<FakeObsState>>,
) {
    let (mut write, mut read) = ws.split();

    // 1) Send Hello (op 0).
    let hello = json!({
        "op": 0,
        "d": {
            "obsWebSocketVersion": "5.6.3",
            "rpcVersion": 1
        }
    });
    if write
        .send(Message::Text(hello.to_string().into()))
        .await
        .is_err()
    {
        return;
    }

    // 2) Wait for Identify (op 1) and reply with Identified (op 2).
    loop {
        match read.next().await {
            Some(Ok(Message::Text(text))) => {
                if let Ok(val) = serde_json::from_str::<Value>(&text) {
                    if val["op"] == 1 {
                        let identified = json!({
                            "op": 2,
                            "d": { "negotiatedRpcVersion": 1 }
                        });
                        if write
                            .send(Message::Text(identified.to_string().into()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        break;
                    }
                }
            }
            Some(Ok(Message::Close(_))) | None => return,
            Some(Err(_)) => return,
            _ => continue,
        }
    }

    // 3) Main loop — respond to requests and forward pushed events.
    let mut event_rx_guard = event_rx.lock().await;
    loop {
        tokio::select! {
            next = read.next() => {
                match next {
                    Some(Ok(Message::Text(text))) => {
                        let Ok(val) = serde_json::from_str::<Value>(&text) else { continue };
                        if val["op"] == 6 {
                            // Simulate the 2026-04-19 transient-failure shape:
                            // a GetInputList request goes out, OBS never responds.
                            let req_type = val["d"]["requestType"].as_str().unwrap_or("");
                            let suppress = {
                                let s = state.lock().await;
                                req_type == "GetInputList" && s.suppress_get_input_list
                            };
                            if suppress {
                                continue;
                            }
                            let response = handle_request(&val, &state).await;
                            if write.send(Message::Text(response.to_string().into())).await.is_err() {
                                return;
                            }
                        }
                    }
                    Some(Ok(Message::Ping(p))) => {
                        let _ = write.send(Message::Pong(p)).await;
                    }
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Err(_)) => return,
                    _ => continue,
                }
            }
            Some(evt) = event_rx_guard.recv() => {
                if write.send(Message::Text(evt.to_string().into())).await.is_err() {
                    return;
                }
            }
        }
    }
}

async fn handle_request(req: &Value, state: &Arc<Mutex<FakeObsState>>) -> Value {
    let request_type = req["d"]["requestType"].as_str().unwrap_or("");
    let request_id = req["d"]["requestId"].as_str().unwrap_or("");

    let response_data = match request_type {
        "GetInputList" => {
            let s = state.lock().await;
            let inputs: Vec<Value> = s
                .inputs
                .iter()
                .map(|(name, kind)| {
                    json!({
                        "inputName": name,
                        "inputKind": kind,
                        "unversionedInputKind": kind,
                    })
                })
                .collect();
            json!({ "inputs": inputs })
        }
        "GetInputSettings" => {
            let input_name = req["d"]["requestData"]["inputName"].as_str().unwrap_or("");
            let s = state.lock().await;
            let settings = s
                .input_settings
                .get(input_name)
                .cloned()
                .unwrap_or_else(|| json!({}));
            let kind = s
                .inputs
                .get(input_name)
                .cloned()
                .unwrap_or_else(|| "unknown".to_string());
            json!({ "inputSettings": settings, "inputKind": kind })
        }
        "GetSceneItemList" => {
            let scene_name = req["d"]["requestData"]["sceneName"].as_str().unwrap_or("");
            let s = state.lock().await;
            let items: Vec<Value> = s
                .scene_items
                .get(scene_name)
                .map(|list| {
                    list.iter()
                        .enumerate()
                        .map(|(i, (name, is_group, kind))| {
                            json!({
                                "sourceName": name,
                                "sceneItemId": i as i64 + 1,
                                "isGroup": is_group,
                                "inputKind": kind,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            json!({ "sceneItems": items })
        }
        _ => json!({}),
    };

    json!({
        "op": 7,
        "d": {
            "requestType": request_type,
            "requestId": request_id,
            "requestStatus": { "result": true, "code": 100 },
            "responseData": response_data,
        }
    })
}

/// Read the next text message from a WebSocket stream, parsed as JSON.
/// Returns `None` if the stream closes first.
pub async fn read_next_json<S>(read: &mut S) -> Option<Value>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    while let Some(next) = read.next().await {
        match next {
            Ok(Message::Text(text)) => {
                if let Ok(val) = serde_json::from_str::<Value>(&text) {
                    return Some(val);
                }
            }
            Ok(Message::Close(_)) => return None,
            Err(_) => return None,
            _ => continue,
        }
    }
    None
}
