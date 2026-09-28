//! OBS WebSocket v5 client with scene detection and text source control.

pub mod dispatcher;
pub mod ndi_discovery;
pub mod ndi_recovery;
pub mod ndi_recovery_io;
pub mod ndi_remove;
pub(crate) mod output_state;
pub mod remote_call;
pub mod scene;
pub mod scene_poll;
pub mod snapshot;
pub mod text;
pub mod transition;

pub use snapshot::ObsSnapshot;
pub use transition::ObsTransition;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use futures::stream::SplitStream;
use futures::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tokio::net::TcpStream;
use tokio::sync::{Notify, RwLock, broadcast, mpsc, watch};
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, warn};

use crate::obs::dispatcher::{DEFAULT_RESPONSE_TIMEOUT, Dispatcher, DispatcherError};
use crate::obs::ndi_discovery::rebuild_ndi_source_map;
use crate::obs::snapshot::ObsShared;
use crate::obs::text::get_current_scene_request;

/// How often the connection loop polls `GetCurrentProgramScene` to reconcile a
/// program-scene change that OBS dropped the `CurrentProgramSceneChanged` event
/// for (#170). Studio Mode can drop that event; a cheap ~2 s poll on the
/// existing WS catches the switch so the wall never sits on a paused source.
const SCENE_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Apply a rebuild result to the shared NDI source map.
///
/// Writes the new map only when `result` is `Some`. A `None` result means
/// the rebuild could not be trusted (typically a transient OBS query
/// failure) and the previous map — even if stale — is kept so scene
/// detection continues to work until the next successful rebuild.
///
/// This is the exact bug that broke the 2026-04-19 event: the old code
/// wrote `new_map` unconditionally, so one failed `GetInputList`
/// wiped the mapping and every subsequent `CurrentProgramSceneChanged`
/// matched against an empty map (→ no engine command → no playback).
pub(crate) async fn apply_rebuild_result(
    ndi_sources: &RwLock<HashMap<String, i64>>,
    result: Option<HashMap<String, i64>>,
) {
    if let Some(new_map) = result {
        let mut guard = ndi_sources.write().await;
        *guard = new_map;
    } else {
        warn!(
            "apply_rebuild_result: rebuild returned None, preserving \
             previous NDI source map (size = {}) so scene detection \
             keeps working on stale data rather than silently breaking",
            ndi_sources.read().await.len()
        );
    }
}

/// Shared OBS connection state.
#[derive(Debug, Clone, Default)]
pub struct ObsState {
    pub connected: bool,
    pub current_scene: Option<String>,
    /// Playlist IDs whose NDI source is currently on program.
    pub active_playlist_ids: HashSet<i64>,
    /// #218: the program scene whose playlist lookup FAILED (always the
    /// `current_scene` when set). `active_playlist_ids` then still holds the
    /// previous set — unknown is not empty — and the ~2 s scene poll looks
    /// the scene up again until it answers.
    pub lookup_failed: Option<String>,
    /// OBS is actively streaming an output (#154). Seeded from
    /// `GetStreamStatus` on connect and updated by `StreamStateChanged`
    /// events; reset to `false` on disconnect. Read by the lyrics idle gate
    /// so heavy GPU work never contends with a live stream.
    pub streaming: bool,
    /// OBS is actively recording an output (#154). Seeded from
    /// `GetRecordStatus` on connect and updated by `RecordStateChanged`
    /// events; reset to `false` on disconnect.
    pub recording: bool,
    /// #219: cg OBS's current scene transition, `None` while unknown (read by
    /// the OBS client at connect and on the transition events, `transition.rs`).
    pub transition: Option<ObsTransition>,
}

impl ObsState {
    /// The connection is gone: nothing of cg OBS is known any more.
    fn reset_disconnected(&mut self) {
        self.connected = false;
        self.current_scene = None;
        self.active_playlist_ids.clear();
        self.lookup_failed = None;
        self.transition = None;
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

/// Mapping of NDI source name to playlist ID (for scene detection).
pub type NdiSourceMap = Arc<RwLock<HashMap<String, i64>>>;

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
    /// #127 / #173: run one rung of the dark-wall recovery ladder for a stranded
    /// NDI receiver. The handler finds the NDI input advertising `ndi_name` (the
    /// bare stream, e.g. `"SP-slow"`) and executes `step` — clear+restore (rung
    /// 0), toggle the scene item (rung 1), or remove+recreate the input (rung 2).
    /// Receiver-side over the healthy OBS WebSocket — never a per-sender
    /// `RecreateSender` (CLAUDE.md "Disabled subsystems", #60).
    NudgeNdiReceiver {
        ndi_name: String,
        step: crate::obs::ndi_recovery::RecoveryStep,
    },
    /// #213: a call of the remote-control facade (`crate::remote`) to cg OBS —
    /// a forwarded obs-websocket request or a scene → playlists lookup — run on
    /// this ONE connection, off the main loop (`remote_call::run`).
    Remote(remote_call::RemoteCall),
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
    /// #213: every op=5 event cg OBS sent, verbatim (`eventType` + `eventData`).
    /// The remote-control facade re-emits the scene ones to its clients.
    Raw {
        event_type: String,
        event_data: serde_json::Value,
    },
}

/// Internal messages from the reader task to the main loop.
enum ReaderMessage {
    /// `CurrentProgramSceneChanged` arrived. Main loop must issue
    /// follow-up GetSceneItemList queries (via dispatcher) and emit
    /// the upstream `ObsEvent::SceneChanged`. `ticket`: its scene ticket,
    /// taken when the reader READ the event (#218 review round 3: during the
    /// connect the main loop is not running yet, so the dequeue order is not
    /// cg OBS's order relative to the initial program read).
    SceneChange { scene_name: String, ticket: u64 },
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
    /// #219: the published program part of `state` (`snapshot.rs`).
    snapshots: watch::Receiver<ObsSnapshot>,
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

        let obs = ObsShared::new(Arc::clone(&state));
        let snapshots = obs.subscribe();
        let loop_event_tx = event_tx.clone();

        tokio::spawn(async move {
            // Aggressive reconnect policy. During a live event, OBS may blip
            // (scene collection switch, briefly unresponsive, etc.) and a
            // 30-second hold-off means lyrics + titles + scene detection are
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
                        &pool,
                        &ndi_sources,
                        &obs,
                        &loop_event_tx,
                        &mut cmd_rx,
                        &mut rebuild_rx,
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

                // Mark disconnected (published) and notify. A fresh scene
                // ticket: an apply of the old connection that still writes
                // after this reset is dropped (#218 review round 3).
                obs.update_scene(obs.scene_ticket(), ObsState::reset_disconnected)
                    .await;
                let _ = loop_event_tx.send(ObsEvent::Disconnected);

                info!("Reconnecting to OBS in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        });

        Self {
            state,
            cmd_tx,
            snapshots,
        }
    }

    /// #219: a receiver of the client's published state (`snapshot.rs`).
    pub fn snapshots(&self) -> watch::Receiver<ObsSnapshot> {
        self.snapshots.clone()
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
/// every inbound message and routes it: op=5 events go to the
/// internal mpsc → main loop (which then issues follow-up queries
/// via the dispatcher), op=7 responses go to the dispatcher's
/// pending map. Other op codes are debug-logged.
///
/// Exits when the WebSocket closes or read errors. Always sends
/// `ReaderMessage::Closed` and calls `dispatcher.drain_and_close()`
/// before returning so no waiter hangs forever and the main loop
/// drops cleanly.
///
/// A `CurrentProgramSceneChanged` gets its scene ticket HERE, in wire order
/// (`obs.scene_ticket()`, #218 review round 3); a transition event wakes the
/// transition reader directly (`transition_wake`, #219 — a `Notify` merges a
/// burst into one read and never blocks this task).
async fn run_reader_task(
    mut read: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    dispatcher: Dispatcher,
    reader_tx: mpsc::Sender<ReaderMessage>,
    event_tx: broadcast::Sender<ObsEvent>,
    obs: ObsShared,
    transition_wake: Arc<Notify>,
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
                        // #213: the remote-control facade re-emits the scene ones.
                        let _ = event_tx.send(ObsEvent::Raw {
                            event_type: event_type.to_string(),
                            event_data: json["d"]["eventData"].clone(),
                        });
                        if event_type == "CurrentProgramSceneChanged"
                            && let Some(scene_name) = json["d"]["eventData"]["sceneName"].as_str()
                        {
                            let _ = reader_tx
                                .send(ReaderMessage::SceneChange {
                                    scene_name: scene_name.to_string(),
                                    ticket: obs.scene_ticket(),
                                })
                                .await;
                        } else if event_type == "StreamStateChanged"
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
                        } else if transition::is_transition_event(event_type) {
                            // #219: re-read cg OBS's transition.
                            transition_wake.notify_one();
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
    pool: &SqlitePool,
    ndi_sources: &NdiSourceMap,
    obs: &ObsShared,
    event_tx: &broadcast::Sender<ObsEvent>,
    cmd_rx: &mut mpsc::Receiver<ObsCommand>,
    rebuild_rx: &mut broadcast::Receiver<()>,
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
    // eventSubscriptions bitmask: Scenes (4) | Transitions (16) | Outputs (64).
    // Outputs delivers StreamStateChanged / RecordStateChanged so the #154 idle
    // gate can defer heavy lyrics work while OBS is live; Transitions delivers
    // CurrentSceneTransitionChanged / CurrentSceneTransitionDurationChanged for
    // the #215 program transition (read by this client, `transition.rs`, #219).
    // 84 = Scenes (4) | Transitions (16) | Outputs (64).
    let mut identify_data = serde_json::json!({
        "rpcVersion": 1,
        "eventSubscriptions": 84
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

    obs.update(|s| s.connected = true).await;
    let _ = event_tx.send(ObsEvent::Connected);

    // Step 4: build dispatcher + spawn reader task.
    // Wrap the write half in Arc<Mutex<>> so tasks spawned from the
    // main loop can each take the lock briefly for a send, then release
    // it before awaiting the op=7 response — concurrent tasks do NOT
    // serialise on the response wait.
    let dispatcher = Dispatcher::new();
    let (reader_tx, mut reader_rx) = mpsc::channel::<ReaderMessage>(32);
    let raw_events = event_tx.clone(); // #213: the reader broadcasts every raw event
    // #219: the reader wakes the transition reader (step 4b) on its events.
    let transition_wake = Arc::new(Notify::new());
    let reader = run_reader_task(
        read,
        dispatcher.clone(),
        reader_tx,
        raw_events,
        obs.clone(),
        Arc::clone(&transition_wake),
    );
    let reader_handle = tokio::spawn(reader);
    let write: SharedWrite = std::sync::Arc::new(tokio::sync::Mutex::new(write));

    // JoinSet tracks all tasks spawned in the main loop body. On loop
    // exit, abort_all() prevents detached tasks from running against a
    // dead write half across reconnects.
    let mut spawned_tasks: JoinSet<()> = JoinSet::new();

    // Step 4b (#219): cg OBS's transition — read now, again on every
    // transition event (the reader wakes it), retried until answered.
    spawn_helper(
        &mut spawned_tasks,
        transition::run_transition_reader(
            Arc::clone(&write),
            dispatcher.clone(),
            obs.clone(),
            transition_wake,
        ),
    );

    // Step 5: initial NDI source map rebuild (same retry-on-empty
    // policy as before — the rebuild now goes via the dispatcher).
    for attempt in 1..=5 {
        let result = rebuild_ndi_source_map(&write, &dispatcher, pool).await;
        let is_empty = result.as_ref().map(|m| m.is_empty()).unwrap_or(true);
        apply_rebuild_result(ndi_sources, result).await;
        if !is_empty {
            break;
        }
        if attempt < 5 {
            warn!("NDI source map empty after rebuild (attempt {attempt}/5); retrying in 2s");
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        } else {
            warn!(
                "NDI source map still empty after 5 rebuild attempts — scene \
                 detection will not work until the next external rebuild signal"
            );
        }
    }

    // Step 6: initial GetCurrentProgramScene via dispatcher. Its scene ticket
    // is taken before the read, like the poll's (`ObsShared::update_scene`).
    let initial_ticket = obs.scene_ticket();
    let initial_scene_req_id = uuid::Uuid::new_v4().to_string();
    let initial_scene_req = get_current_scene_request(&initial_scene_req_id);
    match dispatcher
        .send_and_await(
            &write,
            initial_scene_req_id,
            Message::Text(initial_scene_req.to_string().into()),
            DEFAULT_RESPONSE_TIMEOUT,
        )
        .await
    {
        Ok(response) => {
            if let Some(scene_name) =
                response["d"]["responseData"]["currentProgramSceneName"].as_str()
            {
                // Same seed-the-scene path the reader/poll arms use.
                scene::apply_scene_change(
                    &write,
                    &dispatcher,
                    ndi_sources,
                    obs,
                    event_tx,
                    scene_name.to_string(),
                    initial_ticket,
                )
                .await;
            } else {
                debug!("initial GetCurrentProgramScene response had no scene name");
            }
        }
        Err(DispatcherError::Closed) => {
            // Dispatcher closed before reply — reader task already died.
            // Bail so the outer reconnect loop fires immediately.
            spawned_tasks.abort_all();
            reader_handle.abort();
            if let Err(e) = reader_handle.await
                && !e.is_cancelled()
            {
                warn!("OBS reader task exited with error: {e}");
            }
            anyhow::bail!("reader task closed during initial GetCurrentProgramScene");
        }
        Err(DispatcherError::Timeout) => {
            debug!("initial GetCurrentProgramScene timed out");
        }
    }

    // Step 6b (#154): seed OBS stream/record state so the idle gate knows about
    // an output already active at connect time (no StreamStateChanged/
    // RecordStateChanged fires for it). Best-effort; see `output_state`.
    output_state::seed_output_state(&write, &dispatcher, obs.state()).await;

    // Step 6c (#170): reconcile the program scene by polling
    // `GetCurrentProgramScene` on a ~2 s cadence. In Studio Mode OBS can DROP a
    // `CurrentProgramSceneChanged` — SongPlayer's event stream stays alive but
    // never learns of the switch, leaving the wall on a paused source. Skip
    // missed ticks so a slow round-trip does not burst catch-up requests.
    let mut scene_poll = tokio::time::interval(SCENE_POLL_INTERVAL);
    scene_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // #170: the poll's mismatch clock (polled scene, first seen) across ticks.
    let scene_pending: std::sync::Arc<std::sync::Mutex<Option<(String, std::time::Instant)>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));

    // Step 7: main loop — thin router: each arm spawns a task to do
    // the work. The write half is shared via Arc<Mutex<>> so helper
    // tasks lock it briefly for the send and release before awaiting
    // the op=7 response, preventing the main loop from blocking on
    // in-flight requests.
    let result = loop {
        tokio::select! {
            reader_msg = reader_rx.recv() => {
                match reader_msg {
                    Some(ReaderMessage::SceneChange { scene_name, ticket }) => {
                        // `ticket` was taken by the reader, in wire order.
                        let write = std::sync::Arc::clone(&write);
                        let dispatcher = dispatcher.clone();
                        let ndi_sources = std::sync::Arc::clone(ndi_sources);
                        let obs = obs.clone();
                        let event_tx = event_tx.clone();
                        spawn_helper(&mut spawned_tasks, async move {
                            scene::apply_scene_change(
                                &write,
                                &dispatcher,
                                &ndi_sources,
                                &obs,
                                &event_tx,
                                scene_name,
                                ticket,
                            )
                            .await;
                        });
                    }
                    Some(ReaderMessage::OutputState { recording, active }) => {
                        // #154: record OBS stream/record state for the idle gate.
                        let mut s = obs.state().write().await;
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
                    ObsCommand::NudgeNdiReceiver { ndi_name, step } => {
                        // #127 / #173: SongPlayer detected a stranded DistroAV
                        // receiver (dark wall while Playing). Execute the chosen
                        // ladder rung over this healthy OBS WebSocket. Spawned so
                        // the main loop does not block on the OBS round-trips.
                        let write = std::sync::Arc::clone(&write);
                        let dispatcher = dispatcher.clone();
                        spawn_helper(&mut spawned_tasks, async move {
                            crate::obs::ndi_recovery_io::execute(
                                &write,
                                &dispatcher,
                                &ndi_name,
                                step,
                            )
                            .await;
                        });
                    }
                    ObsCommand::Remote(call) => {
                        // #213: forwarded for the remote-control facade.
                        let write = std::sync::Arc::clone(&write);
                        let ndi_sources = std::sync::Arc::clone(ndi_sources);
                        let dispatcher = dispatcher.clone();
                        spawn_helper(&mut spawned_tasks, remote_call::run(write, dispatcher, ndi_sources, call));
                    }
                }
            }
            rebuild_result = rebuild_rx.recv() => {
                let should_rebuild = match rebuild_result {
                    Ok(()) => {
                        debug!("received rebuild signal, refreshing NDI source map");
                        true
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(
                            "rebuild signal channel lagged by {n} messages, \
                             refreshing NDI source map once"
                        );
                        true
                    }
                    Err(broadcast::error::RecvError::Closed) => false,
                };
                if should_rebuild {
                    let write = std::sync::Arc::clone(&write);
                    let dispatcher = dispatcher.clone();
                    let ndi_sources = std::sync::Arc::clone(ndi_sources);
                    let pool = pool.clone();
                    spawn_helper(&mut spawned_tasks, async move {
                        apply_rebuild_result(
                            &ndi_sources,
                            rebuild_ndi_source_map(&write, &dispatcher, &pool).await,
                        )
                        .await;
                    });
                }
            }
            _ = scene_poll.tick() => {
                // #170: reconcile a program-scene change OBS dropped the event
                // for — read GetCurrentProgramScene and feed the same path the
                // event does when it differs from the last event-derived scene
                // — or, #218, when that scene's playlist lookup failed.
                let write = std::sync::Arc::clone(&write);
                let dispatcher = dispatcher.clone();
                let ndi_sources = std::sync::Arc::clone(ndi_sources);
                let obs = obs.clone();
                let event_tx = event_tx.clone();
                let scene_pending = std::sync::Arc::clone(&scene_pending);
                spawn_helper(&mut spawned_tasks, async move {
                    scene_poll::reconcile_program_scene(
                        &write,
                        &dispatcher,
                        &ndi_sources,
                        &obs,
                        &event_tx,
                        &scene_pending,
                    )
                    .await;
                });
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

/// Spawn one of the connection's helper tasks into `tasks` (review round 4).
fn spawn_helper(
    tasks: &mut JoinSet<()>,
    task: impl std::future::Future<Output = ()> + Send + 'static,
) {
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
