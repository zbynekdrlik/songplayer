//! OBS WebSocket v5 client: SongPlayer's ONE connection to cg OBS.
//!
//! #221 L6: cg OBS is only the NDI input "OBS manuál" now, so this client
//! no longer tracks its program scene or its transition (the scene
//! detection, the ~2 s scene poll, the published `ObsSnapshot` and the
//! transition reader are deleted). It carries:
//!
//! - the remote-control facade's calls (`remote_call.rs`): the forwarded
//!   getters and a manual press's `SetCurrentProgramScene`;
//! - the title text (`SetTextSource`);
//! - cg OBS's raw events for the facade (`ObsEvent::Raw`: only its
//!   `SceneListChanged` is passed on);
//! - `ObsState`: connected + the #154 stream/record state.
//!
//! #221 lane 3 deleted the #127/#173 receiver ladder (`NudgeNdiReceiver`)
//! and the NDI source map with the per-playlist NDI senders whose cg OBS
//! inputs they served.

pub mod dispatcher;
pub(crate) mod output_state;
pub mod remote_call;
pub mod text;

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use futures::stream::SplitStream;
use futures::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tokio::net::TcpStream;
use tokio::sync::{RwLock, broadcast, mpsc};
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, warn};

use crate::obs::dispatcher::{DEFAULT_RESPONSE_TIMEOUT, Dispatcher, DispatcherError};

/// Shared OBS connection state. #221 L6: nothing of cg OBS's program (its
/// scene, the playlists it shows, its transition) is tracked any more.
#[derive(Debug, Clone, Default)]
pub struct ObsState {
    pub connected: bool,
    /// OBS is actively streaming an output (#154). Seeded from
    /// `GetStreamStatus` on connect and updated by `StreamStateChanged`
    /// events; reset to `false` on disconnect. Read by the lyrics idle gate
    /// so heavy GPU work never contends with a live stream.
    pub streaming: bool,
    /// OBS is actively recording an output (#154). Seeded from
    /// `GetRecordStatus` on connect and updated by `RecordStateChanged`
    /// events; reset to `false` on disconnect.
    pub recording: bool,
}

impl ObsState {
    /// The connection is gone: nothing of cg OBS is known any more.
    fn reset_disconnected(&mut self) {
        self.connected = false;
        // #154: OBS is gone — its stream/record state is unknown, so clear it.
        // The Playing gate still covers SP outputs; a stale `true` here would
        // gate heavy work forever.
        self.streaming = false;
        self.recording = false;
    }
}

/// Configuration for connecting to OBS WebSocket.
#[derive(Debug, Clone, PartialEq)]
pub struct ObsConfig {
    /// WebSocket URL, e.g. `"ws://127.0.0.1:4455"`.
    pub url: String,
    /// Optional password for authentication.
    pub password: Option<String>,
}

/// The OBS connection settings as the dashboard stores them (Nastavenia):
/// `None` when no WebSocket URL is set, an empty password means no auth.
pub async fn load_obs_config(pool: &SqlitePool) -> Result<Option<ObsConfig>, sqlx::Error> {
    use crate::db::models::get_setting;
    let url = get_setting(pool, sp_core::config::SETTING_OBS_WEBSOCKET_URL)
        .await?
        .unwrap_or_default();
    if url.is_empty() {
        return Ok(None);
    }
    let password = get_setting(pool, sp_core::config::SETTING_OBS_WEBSOCKET_PASSWORD)
        .await?
        .unwrap_or_default();
    Ok(Some(ObsConfig {
        url,
        password: (!password.is_empty()).then_some(password),
    }))
}

/// Shared writer for the OBS WebSocket — wrapped in an Arc + Mutex so
/// helper tasks spawned from the main loop can take turns sending
/// requests without serialising on the response-await.
///
/// Note: this is `tokio::sync::Mutex`, NOT `std::sync::Mutex`. The lock
/// is held across `.await` so it must be the async variant.
pub(crate) type SharedWrite = std::sync::Arc<
    tokio::sync::Mutex<
        futures::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            tokio_tungstenite::tungstenite::Message,
        >,
    >,
>;

/// Commands that can be sent to the OBS WebSocket connection loop.
#[derive(Debug)]
pub enum ObsCommand {
    SetTextSource {
        source_name: String,
        text: String,
    },
    /// #213: a call of the remote-control facade (`crate::remote`) to cg OBS —
    /// a forwarded obs-websocket request, run on this ONE connection by its ONE
    /// forwarder (`remote_call::run_calls`, #221: in queue order, a scene
    /// switch answered before the next call is written).
    Remote(remote_call::RemoteCall),
}

/// cg OBS's events, as the connection loop's reader receives them.
#[derive(Debug, Clone)]
pub enum ObsEvent {
    /// #213: every op=5 event cg OBS sent, verbatim (`eventType` + `eventData`).
    /// The remote-control facade re-emits `SceneListChanged` to its clients.
    Raw {
        event_type: String,
        event_data: serde_json::Value,
    },
}

/// Internal messages from the reader task to the main loop.
enum ReaderMessage {
    /// #154: `StreamStateChanged` / `RecordStateChanged` arrived. `outputActive`
    /// is the new state; the main loop writes it into `ObsState` so the lyrics
    /// idle gate defers heavy work while OBS is live. `recording` distinguishes
    /// the two output kinds.
    OutputState { recording: bool, active: bool },
    /// Stream closed cleanly OR errored. Main loop must exit so the
    /// outer reconnect loop fires.
    Closed,
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
    pub fn spawn(
        config: ObsConfig,
        shared_state: Arc<RwLock<ObsState>>,
        event_tx: broadcast::Sender<ObsEvent>,
        mut shutdown: broadcast::Receiver<()>,
    ) -> Self {
        let state = shared_state;
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<ObsCommand>(64);

        let loop_state = Arc::clone(&state);
        let loop_event_tx = event_tx.clone();

        tokio::spawn(async move {
            // Aggressive reconnect policy. During a live event, OBS may blip
            // (scene collection switch, briefly unresponsive, etc.) and a
            // 30-second hold-off means the titles and a manual press are
            // dark for 30+ seconds every time. Cap at 5 s so reconnect is
            // human-imperceptible. First attempt is immediate (start=1 s).
            let mut backoff = Duration::from_secs(1);
            const MAX_BACKOFF: Duration = Duration::from_secs(5);

            loop {
                tokio::select! {
                    _ = shutdown.recv() => {
                        info!("OBS client shutting down");
                        break;
                    }
                    result = connect_and_run(
                        &config,
                        &loop_state,
                        &loop_event_tx,
                        &mut cmd_rx,
                    ) => {
                        // #80: every disconnect, including a clean
                        // server-side close, MUST fall through to the
                        // backoff + reconnect path. The only terminal
                        // state for this loop is an explicit
                        // shutdown.recv() (handled in the other select
                        // arm). Previously `Ok(()) => break` left
                        // SongPlayer permanently OBS-deaf on a 2026-05-03
                        // clean-close in production.
                        match result {
                            Ok(()) => info!("OBS connection closed cleanly; will reconnect"),
                            Err(e) => warn!("OBS connection error: {e}"),
                        }
                    }
                }

                // Mark disconnected.
                loop_state.write().await.reset_disconnected();

                info!("Reconnecting to OBS in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        });

        Self { state, cmd_tx }
    }

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

/// The `eventSubscriptions` of the client's `Identify`: Scenes (4) |
/// Outputs (64). Pinned by `mod_tests.rs`.
const EVENT_SUBSCRIPTIONS: u64 = 68;

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

/// Reader task: owns the SplitStream<read> after handshake. Reads
/// every inbound message and routes it: every op=5 event is broadcast raw
/// (`ObsEvent::Raw`) and the #154 output ones also go to the main loop,
/// op=7 responses go to the dispatcher's pending map. Other op codes are
/// debug-logged.
///
/// Exits when the WebSocket closes or read errors. Always sends
/// `ReaderMessage::Closed` and calls `dispatcher.drain_and_close()`
/// before returning so no waiter hangs forever and the main loop
/// drops cleanly.
async fn run_reader_task(
    mut read: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    dispatcher: Dispatcher,
    reader_tx: mpsc::Sender<ReaderMessage>,
    event_tx: broadcast::Sender<ObsEvent>,
) {
    loop {
        match read.next().await {
            Some(Ok(Message::Text(text))) => {
                let json: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("OBS reader: invalid JSON: {e}");
                        continue;
                    }
                };
                let op = json["op"].as_u64().unwrap_or(u64::MAX);
                match op {
                    5 => {
                        let event_type = json["d"]["eventType"].as_str().unwrap_or("");
                        debug!("OBS event: {event_type}");
                        // #213: the remote-control facade re-emits the scene list.
                        let _ = event_tx.send(ObsEvent::Raw {
                            event_type: event_type.to_string(),
                            event_data: json["d"]["eventData"].clone(),
                        });
                        if event_type == "StreamStateChanged"
                            && let Some(active) = json["d"]["eventData"]["outputActive"].as_bool()
                        {
                            // #154: OBS started/stopped streaming.
                            let _ = reader_tx
                                .send(ReaderMessage::OutputState {
                                    recording: false,
                                    active,
                                })
                                .await;
                        } else if event_type == "RecordStateChanged"
                            && let Some(active) = json["d"]["eventData"]["outputActive"].as_bool()
                        {
                            // #154: OBS started/stopped recording.
                            let _ = reader_tx
                                .send(ReaderMessage::OutputState {
                                    recording: true,
                                    active,
                                })
                                .await;
                        }
                    }
                    7 => {
                        let req_id = json["d"]["requestId"].as_str().unwrap_or("").to_string();
                        if req_id.is_empty() {
                            warn!("OBS reader: op=7 without requestId, dropping");
                            continue;
                        }
                        dispatcher.complete(&req_id, json);
                    }
                    _ => {
                        debug!("unhandled OBS message op={op}");
                    }
                }
            }
            Some(Ok(Message::Close(_))) | None => {
                info!("OBS WebSocket closed");
                break;
            }
            Some(Ok(_)) => {} // ping/pong/binary
            Some(Err(e)) => {
                warn!("OBS reader: stream error: {e}");
                break;
            }
        }
    }

    dispatcher.drain_and_close();
    let _ = reader_tx.send(ReaderMessage::Closed).await;
}

/// Main connection loop: connect, authenticate, handle messages.
async fn connect_and_run(
    config: &ObsConfig,
    state: &Arc<RwLock<ObsState>>,
    event_tx: &broadcast::Sender<ObsEvent>,
    cmd_rx: &mut mpsc::Receiver<ObsCommand>,
) -> Result<(), anyhow::Error> {
    let (ws_stream, _) = tokio_tungstenite::connect_async(&config.url).await?;
    let (mut write, mut read) = ws_stream.split();

    // Step 1: Hello (op 0).
    let hello = read_json_message(&mut read).await?;
    let op = hello["op"].as_u64().unwrap_or(u64::MAX);
    if op != 0 {
        anyhow::bail!("expected Hello (op 0), got op {op}");
    }
    debug!("received OBS Hello");

    // Step 2: Identify (op 1).
    // eventSubscriptions bitmask: Scenes (4) delivers SceneListChanged, which
    // the remote-control facade passes on to Companion; Outputs (64) delivers
    // StreamStateChanged / RecordStateChanged so the #154 idle gate can defer
    // heavy lyrics work while OBS is live. #221 L6 dropped Transitions (16):
    // cg OBS's transition is no longer read.
    let mut identify_data = serde_json::json!({
        "rpcVersion": 1,
        "eventSubscriptions": EVENT_SUBSCRIPTIONS
    });
    if let Some(password) = &config.password
        && let Some(auth) = hello["d"]["authentication"].as_object()
    {
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
    let identify_msg = serde_json::json!({"op": 1, "d": identify_data});
    write
        .send(Message::Text(identify_msg.to_string().into()))
        .await?;

    // Step 3: Identified (op 2).
    let identified = read_json_message(&mut read).await?;
    let op = identified["op"].as_u64().unwrap_or(u64::MAX);
    if op != 2 {
        anyhow::bail!("expected Identified (op 2), got op {op}");
    }
    info!("connected to OBS WebSocket");

    state.write().await.connected = true;

    // Step 4: build dispatcher + spawn reader task.
    // Wrap the write half in Arc<Mutex<>> so tasks spawned from the
    // main loop can each take the lock briefly for a send, then release
    // it before awaiting the op=7 response — concurrent tasks do NOT
    // serialise on the response wait.
    let dispatcher = Dispatcher::new();
    let (reader_tx, mut reader_rx) = mpsc::channel::<ReaderMessage>(32);
    let raw_events = event_tx.clone(); // #213: the reader broadcasts every raw event
    let reader = run_reader_task(read, dispatcher.clone(), reader_tx, raw_events);
    let reader_handle = tokio::spawn(reader);
    let write: SharedWrite = std::sync::Arc::new(tokio::sync::Mutex::new(write));

    // JoinSet tracks the connection's helper tasks: the facade's forwarder
    // and every task the main loop body spawns (`spawn_helper`, which reaps
    // the finished ones). On loop exit, abort_all() prevents them from
    // running against a dead write half across reconnects.
    let mut spawned_tasks: JoinSet<()> = JoinSet::new();

    // #221: ONE forwarder writes the remote-control facade's calls in order.
    let (remote_tx, forwarder) = remote_call::forwarder(Arc::clone(&write), dispatcher.clone());
    spawn_helper(&mut spawned_tasks, forwarder);

    // Step 5 (#154): seed OBS stream/record state so the idle gate knows about
    // an output already active at connect time (no StreamStateChanged/
    // RecordStateChanged fires for it). Best-effort; see `output_state`.
    output_state::seed_output_state(&write, &dispatcher, state).await;

    // Step 6: main loop — thin router: each arm spawns a task (a Remote
    // call goes to the connection's forwarder) to do the work. The write
    // half is shared via Arc<Mutex<>> so helper tasks lock it briefly for
    // the send and release before awaiting the op=7 response, preventing
    // the main loop from blocking on in-flight requests.
    let result = loop {
        tokio::select! {
            reader_msg = reader_rx.recv() => {
                match reader_msg {
                    Some(ReaderMessage::OutputState { recording, active }) => {
                        // #154: record OBS stream/record state for the idle gate.
                        let mut s = state.write().await;
                        if recording {
                            s.recording = active;
                        } else {
                            s.streaming = active;
                        }
                        debug!(
                            recording,
                            active, "OBS output state changed (idle-gate signal)"
                        );
                    }
                    Some(ReaderMessage::Closed) | None => {
                        break Ok(());
                    }
                }
            }
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    ObsCommand::SetTextSource { source_name, text } => {
                        let write = std::sync::Arc::clone(&write);
                        let dispatcher = dispatcher.clone();
                        spawn_helper(&mut spawned_tasks, async move {
                            let req_id = uuid::Uuid::new_v4().to_string();
                            let req = text::set_text_request(&req_id, &source_name, &text);
                            match dispatcher
                                .send_and_await(
                                    &write,
                                    req_id,
                                    Message::Text(req.to_string().into()),
                                    DEFAULT_RESPONSE_TIMEOUT,
                                )
                                .await
                            {
                                Ok(response) => {
                                    let ok = response["d"]["requestStatus"]["result"]
                                        .as_bool()
                                        .unwrap_or(false);
                                    if ok {
                                        info!(source_name, "SetTextSource ok");
                                    } else {
                                        let code = response["d"]["requestStatus"]["code"]
                                            .as_u64()
                                            .unwrap_or(0);
                                        let comment = response["d"]["requestStatus"]["comment"]
                                            .as_str()
                                            .unwrap_or("");
                                        warn!(
                                            source_name,
                                            code,
                                            comment,
                                            "SetTextSource: OBS reported failure"
                                        );
                                    }
                                }
                                Err(DispatcherError::Closed) => {
                                    warn!(
                                        source_name,
                                        "SetTextSource: dispatcher closed before reply"
                                    );
                                }
                                Err(DispatcherError::Timeout) => {
                                    warn!(source_name, "SetTextSource: timed out");
                                }
                            }
                        });
                    }
                    ObsCommand::Remote(call) => {
                        // #213/#221: to this connection's forwarder, in order.
                        if remote_tx.send(call).is_err() {
                            warn!("remote: the forwarder of this OBS connection is gone — call dropped");
                        }
                    }
                }
            }
        }
    };

    spawned_tasks.abort_all();
    reader_handle.abort();
    // Reader task exit observation: aborted handles return JoinError, panics
    // surface here. We don't propagate but DO log so silent reader panics are
    // visible in logs.
    if let Err(e) = reader_handle.await
        && !e.is_cancelled()
    {
        warn!("OBS reader task exited with error: {e}");
    }
    result
}

/// Spawn one of the connection's helper tasks into `tasks`, first reaping the
/// finished ones (review round 4): a `JoinSet` keeps a finished task until it
/// is joined, and the connection loop never joins (one helper per title
/// text, for the life of the connection). A helper
/// that panicked is logged here.
fn spawn_helper(
    tasks: &mut JoinSet<()>,
    task: impl std::future::Future<Output = ()> + Send + 'static,
) {
    while let Some(done) = tasks.try_join_next() {
        if let Err(e) = done
            && !e.is_cancelled()
        {
            warn!("OBS helper task failed: {e}");
        }
    }
    tasks.spawn(task);
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
#[path = "mod_tests.rs"]
mod tests;
