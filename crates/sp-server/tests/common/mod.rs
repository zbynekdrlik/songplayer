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
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

/// The key of the control message `FakeObsServer::release_held` sends down
/// the event channel (never forwarded to the client as an event).
const RELEASE_HELD: &str = "__release_held";
/// The key of the control message `FakeObsServer::close_client` sends down the
/// event channel: close the connected client's WebSocket.
const CLOSE_CLIENT: &str = "__close_client";

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
    /// When true, the fake server sends a WebSocket Close frame immediately
    /// after replying with `Identified`. This reproduces the 2026-05-03
    /// production failure mode behind #80: a clean server-side close
    /// caused the reconnect loop to terminate instead of backing off and
    /// reconnecting.
    pub close_after_identify: bool,
    /// #213: the scenes `GetSceneList` lists, in order (their uuid is
    /// `uuid-<name>`). `SetCurrentProgramScene` accepts only these.
    pub scene_list: Vec<String>,
    /// #213: the program scene (`GetSceneList` / `GetCurrentProgramScene`;
    /// `SetCurrentProgramScene` sets it). `None` keeps the old `{}` answer
    /// of `GetCurrentProgramScene`.
    pub program_scene: Option<String>,
    /// #213: every request received, as `{requestType, requestData}`.
    pub requests: Vec<Value>,
    /// #218: the next N `GetSceneItemList` requests get NO answer (the
    /// client's lookup times out). Each dropped request is still logged in
    /// `requests`, with `"dropped": true`.
    pub drop_scene_item_lists: usize,
    /// #218: the next N `GetSceneItemList` requests are answered with a
    /// success status but no `sceneItems` list.
    pub omit_scene_items: usize,
    /// #218: the next N `GetSceneItemList` requests are REFUSED (600, as for
    /// an unknown scene).
    pub refuse_scene_item_lists: usize,
    /// #218: sources that are GROUPS: `GetSceneItemList` for one is refused
    /// with 602 "(Is group)", as obs-websocket 5 does (a group is listed only
    /// by `GetGroupSceneItemList`). List them in `scene_items` as
    /// `(name, true, "")`.
    pub groups: Vec<String>,
    /// #218 review round 2: while set, every `GetCurrentProgramScene` answer
    /// is built at once (the program scene at that moment) but HELD in
    /// `held` until [`FakeObsServer::release_held`].
    pub hold_program_scene: bool,
    /// #218 review round 2: while set, the `GetSceneItemList` answers for this
    /// scene are held the same way.
    pub hold_lookups_of: Option<String>,
    /// The held answers, in request order.
    pub held: Vec<Value>,
    /// #218 review round 3: right after answering the client's first
    /// `GetInputList` (its connect-time NDI map rebuild, before its initial
    /// `GetCurrentProgramScene`), send `CurrentProgramSceneChanged` for this
    /// scene — an event cg OBS sent before the client's initial read.
    pub event_on_input_list: Option<String>,
    /// #219: the `responseData` of `GetCurrentSceneTransition`; `None` answers
    /// `{}` (no transition kind: the client reads it as no answer).
    pub scene_transition: Option<Value>,
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

    /// #218: send the held answers of `request_type` to the client, in order
    /// (after anything pushed before this call).
    pub async fn release_held(&self, request_type: &str) {
        let _ = self
            .event_tx
            .send(json!({ RELEASE_HELD: request_type }))
            .await;
    }

    /// #219: close the connected client's WebSocket (a Close frame) — cg OBS
    /// going away; the fake keeps accepting, so the client reconnects.
    pub async fn close_client(&self) {
        let _ = self.event_tx.send(json!({ CLOSE_CLIENT: true })).await;
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
                            // Simulate the 2026-04-19 transient-failure shape:
                            // a GetInputList request goes out, OBS never responds.
                            let req_type = val["d"]["requestType"].as_str().unwrap_or("");
                            let suppress = {
                                let mut s = state.lock().await;
                                if req_type == "GetSceneItemList" && s.drop_scene_item_lists > 0 {
                                    // #218: cg OBS never answers this lookup.
                                    s.drop_scene_item_lists -= 1;
                                    s.requests.push(json!({
                                        "requestType": req_type,
                                        "requestData": val["d"]["requestData"],
                                        "dropped": true,
                                    }));
                                    true
                                } else {
                                    req_type == "GetInputList" && s.suppress_get_input_list
                                }
                            };
                            if suppress {
                                continue;
                            }
                            let response = handle_request(&val, &state).await;
                            {
                                // #218: an answer built NOW (the state at
                                // processing time), sent only on `release_held`.
                                let mut s = state.lock().await;
                                let scene = val["d"]["requestData"]["sceneName"].as_str();
                                let hold = (req_type == "GetCurrentProgramScene"
                                    && s.hold_program_scene)
                                    || (req_type == "GetSceneItemList"
                                        && s.hold_lookups_of.is_some()
                                        && s.hold_lookups_of.as_deref() == scene);
                                if hold {
                                    s.held.push(response);
                                    continue;
                                }
                            }
                            if write.send(Message::Text(response.to_string().into())).await.is_err() {
                                return;
                            }
                            // #218 review round 3: cg OBS switched its program
                            // while the client was still connecting (its NDI
                            // map rebuild runs first).
                            let early = if req_type == "GetInputList" {
                                state.lock().await.event_on_input_list.take()
                            } else {
                                None
                            };
                            if let Some(scene) = early {
                                let evt = json!({
                                    "op": 5,
                                    "d": {
                                        "eventType": "CurrentProgramSceneChanged",
                                        "eventIntent": 4,
                                        "eventData": { "sceneName": scene }
                                    }
                                });
                                if write.send(Message::Text(evt.to_string().into())).await.is_err() {
                                    return;
                                }
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
                // #219: a `close_client` control message.
                if evt.get(CLOSE_CLIENT).is_some() {
                    let _ = write.send(Message::Close(None)).await;
                    return;
                }
                // #218: a `release_held` control message, not an OBS event.
                if let Some(kind) = evt.get(RELEASE_HELD).and_then(Value::as_str) {
                    let released: Vec<Value> = {
                        let mut s = state.lock().await;
                        let (out, keep): (Vec<Value>, Vec<Value>) = s
                            .held
                            .drain(..)
                            .partition(|r| r["d"]["requestType"] == kind);
                        s.held = keep;
                        out
                    };
                    for response in released {
                        if write.send(Message::Text(response.to_string().into())).await.is_err() {
                            return;
                        }
                    }
                    continue;
                }
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
        "GetSceneItemList" => {
            let scene_name = req["d"]["requestData"]["sceneName"].as_str().unwrap_or("");
            let mut s = state.lock().await;
            let group = s.groups.iter().any(|g| g == scene_name);
            if group || s.refuse_scene_item_lists > 0 {
                // #218: a refusal carries no responseData at all.
                let (code, comment) = if group {
                    (
                        602,
                        "The specified source is not a scene. (Is group)".to_string(),
                    )
                } else {
                    s.refuse_scene_item_lists -= 1;
                    (
                        600,
                        format!("No source was found by the name of `{scene_name}`."),
                    )
                };
                return json!({
                    "op": 7,
                    "d": {
                        "requestType": request_type,
                        "requestId": request_id,
                        "requestStatus": { "result": false, "code": code, "comment": comment },
                    }
                });
            }
            if s.omit_scene_items > 0 {
                // #218: a success status, but no `sceneItems` list.
                s.omit_scene_items -= 1;
                return json!({
                    "op": 7,
                    "d": {
                        "requestType": request_type,
                        "requestId": request_id,
                        "requestStatus": request_status,
                        "responseData": {},
                    }
                });
            }
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
        "GetCurrentSceneTransition" => state
            .lock()
            .await
            .scene_transition
            .clone()
            .unwrap_or_else(|| json!({})),
        "GetCurrentProgramScene" => match state.lock().await.program_scene.clone() {
            Some(name) => json!({
                "sceneName": name,
                "sceneUuid": format!("uuid-{name}"),
                "currentProgramSceneName": name,
                "currentProgramSceneUuid": format!("uuid-{name}"),
            }),
            None => json!({}),
        },
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
