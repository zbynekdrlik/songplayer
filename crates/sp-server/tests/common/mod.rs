//! Shared test harness — `FakeObsServer` that speaks enough of the OBS
//! WebSocket 5.x protocol to drive sp-server's OBS client in integration
//! tests without a real OBS process.
//!
//! Covers:
//! - Hello (op 0) → Identify (op 1) → Identified (op 2) handshake with no auth.
//! - RequestResponse (op 7) replies to `GetInputList`, `GetInputSettings`,
//!   `GetSceneList` and `SetCurrentProgramScene` (anything else: an empty
//!   success).
//! - Pushing events (op 5) via a control channel.
//!
//! #221 L6 deleted the OBS client's scene detection and transition reader,
//! and with them this harness's knobs for their tests (the scene items and
//! #218's failed / held lookups, #219's transition and client close).
//!
//! Rust convention: files under `tests/common/` are automatically excluded
//! from the integration-test binary list, so each test file can `mod common;`
//! without spawning a dead binary.

#![allow(dead_code)] // Not every helper is used by every test file.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

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
    /// When true, the fake server sends a WebSocket Close frame immediately
    /// after replying with `Identified`. This reproduces the 2026-05-03
    /// production failure mode behind #80: a clean server-side close
    /// caused the reconnect loop to terminate instead of backing off and
    /// reconnecting.
    pub close_after_identify: bool,
    /// #213: the scenes `GetSceneList` lists, in order (their uuid is
    /// `uuid-<name>`). `SetCurrentProgramScene` accepts only these.
    pub scene_list: Vec<String>,
    /// #213: cg OBS's own program scene (`GetSceneList`;
    /// `SetCurrentProgramScene` sets it).
    pub program_scene: Option<String>,
    /// #213: every request received, as `{requestType, requestData}`.
    pub requests: Vec<Value>,
}

/// A fake OBS WebSocket server listening on a random localhost port.
pub struct FakeObsServer {
    addr: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    event_tx: mpsc::Sender<Value>,
    state: Arc<Mutex<FakeObsState>>,
    /// Incremented each time the accept loop completes a TCP accept(). Used
    /// by reconnect tests to assert the client reopened the connection.
    accept_count: Arc<AtomicUsize>,
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
        let accept_count = Arc::new(AtomicUsize::new(0));
        let accept_count_clone = accept_count.clone();

        tokio::spawn(async move {
            run_accept_loop(
                listener,
                shutdown_rx,
                event_rx,
                state_clone,
                accept_count_clone,
            )
            .await;
        });

        Self {
            addr,
            shutdown_tx: Some(shutdown_tx),
            event_tx,
            state,
            accept_count,
        }
    }

    /// How many TCP connections has the accept loop served? Used by the
    /// reconnect tests to assert that after a clean close the client
    /// reopened the connection (count >= 2) instead of giving up (count == 1).
    pub fn accept_count(&self) -> usize {
        self.accept_count.load(Ordering::SeqCst)
    }

    /// WebSocket URL clients should connect to.
    pub fn url(&self) -> String {
        format!("ws://{}", self.addr)
    }

    /// #213: push any event (intent 4 = Scenes) to the connected client.
    pub async fn push_event(&self, event_type: &str, event_data: Value) {
        let evt = json!({
            "op": 5,
            "d": { "eventType": event_type, "eventIntent": 4, "eventData": event_data }
        });
        let _ = self.event_tx.send(evt).await;
    }

    /// #213: a snapshot of the fake state (request log, program scene, …).
    pub async fn state(&self) -> FakeObsState {
        self.state.lock().await.clone()
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
    accept_count: Arc<AtomicUsize>,
) {
    // Wrap event_rx in a Mutex so `handle_client` can borrow it when a client connects.
    let event_rx = Arc::new(Mutex::new(event_rx));

    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((tcp, _)) => {
                        accept_count.fetch_add(1, Ordering::SeqCst);
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
                        let close_now = { state.lock().await.close_after_identify };
                        if close_now {
                            let _ = write.send(Message::Close(None)).await;
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
    state.lock().await.requests.push(json!({
        "requestType": request_type,
        "requestData": req["d"]["requestData"],
    }));
    let mut request_status = json!({ "result": true, "code": 100 });

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
        "GetSceneList" => {
            let s = state.lock().await;
            let scenes: Vec<Value> = s
                .scene_list
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    json!({ "sceneIndex": i, "sceneName": name, "sceneUuid": format!("uuid-{name}") })
                })
                .collect();
            json!({
                "currentProgramSceneName": s.program_scene,
                "currentProgramSceneUuid": s.program_scene.as_ref().map(|n| format!("uuid-{n}")),
                "currentPreviewSceneName": null,
                "currentPreviewSceneUuid": null,
                "scenes": scenes,
            })
        }
        "SetCurrentProgramScene" => {
            let scene = req["d"]["requestData"]["sceneName"].as_str().unwrap_or("");
            let mut s = state.lock().await;
            if s.scene_list.iter().any(|n| n == scene) {
                s.program_scene = Some(scene.to_string());
            } else {
                request_status = json!({
                    "result": false,
                    "code": 600,
                    "comment": format!("No source was found by the name of `{scene}`."),
                });
            }
            json!({})
        }
        _ => json!({}),
    };

    json!({
        "op": 7,
        "d": {
            "requestType": request_type,
            "requestId": request_id,
            "requestStatus": request_status,
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
