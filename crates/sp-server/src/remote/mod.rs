//! Companion-compatible remote control (#213, C of EPIC #174): an
//! obs-websocket 5 subset served by SongPlayer, so the Stream Deck buttons
//! that switch cg OBS scenes through Companion's OBS module cut `SP-program`
//! (#209) once Companion's OBS connection points at SongPlayer — no button is
//! rebuilt. Design record: #213 comment 5850413908 (Approach 1).
//!
//! - **Listener** — its own port ([`DEFAULT_REMOTE_WS_PORT`], setting
//!   `remote_ws_port`), off by default (`remote_ws_enabled`), with the
//!   obs-websocket SHA-256 challenge auth when `remote_ws_password` is set.
//!   [`run_remote_config_task`] re-reads the settings every
//!   [`REMOTE_SETTINGS_POLL`] and (re)binds or stops the listener; a bind
//!   failure is retried on every poll and shown as `remote.error`.
//! - **Sessions** (`session.rs`) — `Hello` / `Identify` / `Identified`,
//!   requests, batches, events. The message objects are `protocol.rs`
//!   (pure); their encoding is `codec.rs` (#221 L2b, pure): JSON text frames
//!   (`obswebsocket.json`) or MessagePack binary frames
//!   (`obswebsocket.msgpack`, what Companion speaks), one per session.
//! - **cg OBS** is reached through SongPlayer's EXISTING OBS client
//!   ([`Upstream`]: its command channel + its event broadcast). The scene and
//!   input list getters are forwarded, so the button names match cg OBS 1:1
//!   (#221 lane 2: `GetSceneList` with SP-program's scene and the session's
//!   preview, `protocol::with_songplayer_scenes`); cg OBS's
//!   `SceneListChanged` is re-emitted to the clients.
//! - **Program feedback (#221 L3)** is SongPlayer's own (`studio_events`):
//!   `CurrentProgramSceneChanged` whenever the scene name of what `SP-program`
//!   has on air changes (a press, a dashboard cut, anything that cuts), and
//!   `SceneTransitionStarted` / `SceneTransitionEnded` around every facade
//!   switch that cut. `GetCurrentProgramScene` answers SP-program's scene,
//!   and so does `GetSceneList`'s `currentProgramSceneName` (lane 2: what
//!   Companion reads at connect). cg OBS's own `CurrentProgramSceneChanged`
//!   is never passed through.
//! - **Studio mode (#221 L2).** Studio mode is reported ON, so Companion's
//!   page-13 buttons (`preview_scene` + `do_transition`) reach SongPlayer:
//!   `SetCurrentPreviewScene` / `GetCurrentPreviewScene` keep a preview PER
//!   SESSION (initially the program scene), and `TriggerStudioModeTransition`
//!   ALWAYS switches to it. `SetCurrentSceneTransitionDuration` is validated
//!   and acknowledged but not applied (the program transition is the Settings
//!   value): `remote.last_transition_duration {ms, applied: false}`.
//! - **A scene press** (`SetCurrentProgramScene`, or the transition) goes
//!   through the ONE switch path, `playback::program_switch`: a playlist
//!   scene (from SongPlayer's own catalog) is cut and cg OBS is told nothing
//!   (#221 B4 step 6), a manual scene goes to cg OBS first, then "OBS
//!   manuál" (#212). Presses are applied one at a time, in arrival order,
//!   across every client.
//! - **Telemetry** lives on the program bus ([`RemoteShared`],
//!   `ProgramBus::remote()`) and is served as `remote` on `GET /api/v1/program`.

pub mod codec;
pub mod protocol;
mod session;
pub mod studio_events;

use std::collections::BTreeSet;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use sp_core::config::{
    DEFAULT_REMOTE_WS_PORT, SETTING_REMOTE_WS_ENABLED, SETTING_REMOTE_WS_PASSWORD,
    SETTING_REMOTE_WS_PORT,
};
use sqlx::SqlitePool;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, broadcast, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{info, warn};

use crate::obs::remote_call::RemoteCall;
use crate::obs::{ObsCommand, ObsEvent};
use crate::playback::program_bus::ProgramBus;
use crate::playback::program_on_air::{OnAir, program_scene_name};
use studio_events::{FACADE_EVENTS_CAPACITY, FacadeEvent, TRANSITION_END_MAX_WAIT};

/// How often the settings task re-reads the settings.
pub const REMOTE_SETTINGS_POLL: Duration = Duration::from_secs(5);
/// How long a call to cg OBS may take before the client is answered "not
/// ready" (above the OBS client's own 2 s response timeout). A call queued
/// behind a scene switch still in flight first waits for that switch's answer
/// (at most 2 s, `obs::remote_call`); a manual press's switch with less than
/// the OBS client's 2 s of this left is answered "not ready" and never
/// written.
pub const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(3);
/// The pause after a failed `accept` (never a hot loop on e.g. EMFILE).
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
/// A client that has not completed the WebSocket handshake AND identified
/// within this is dropped / closed (an idle unauthenticated socket is never
/// kept).
pub const IDENTIFY_TIMEOUT: Duration = Duration::from_secs(10);
/// The largest message / frame a client may send (1 MiB), text (JSON) or
/// binary (msgpack, #221 L2b) alike. Companion's biggest message is a batch
/// of a few KB; tungstenite's default would be 64 MiB.
pub const MAX_MESSAGE_BYTES: usize = 1_048_576;
/// At most this many unsupported request types are remembered (client-chosen
/// strings on an open LAN surface must stay bounded).
pub const MAX_UNSUPPORTED_LISTED: usize = 64;
/// Client-chosen strings (request types, scene names) are stored and logged
/// clipped to this many characters (`clip`).
pub const MAX_REQUEST_TYPE_CHARS: usize = 64;
/// #221 (main-session decision 5882671183): at most this many sessions at
/// once per listener (Companion needs one, the post-deploy E2E two). A
/// handshake over it is refused with HTTP 503 before it becomes a session:
/// each session may decode a 1 MiB frame, so their number bounds the memory
/// an open LAN port can be made to hold.
pub const MAX_SESSIONS: usize = 16;
/// #221: a handshake refused over [`MAX_SESSIONS`] is logged (INFO) at most
/// once per this interval; `remote.refused_over_cap` counts every one.
pub const REFUSAL_LOG_INTERVAL: Duration = Duration::from_secs(10);

/// The stored remote-control settings.
#[derive(Clone, PartialEq, Eq)]
pub struct RemoteSettings {
    pub enabled: bool,
    pub port: u16,
    /// `None` = no auth (an empty or whitespace-only setting).
    pub password: Option<String>,
}

impl RemoteSettings {
    /// Off, on the default port, no password (an unreadable setting).
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            port: DEFAULT_REMOTE_WS_PORT,
            password: None,
        }
    }
}

/// The password never reaches a log: `Debug` shows only whether one is set.
impl std::fmt::Debug for RemoteSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteSettings")
            .field("enabled", &self.enabled)
            .field("port", &self.port)
            .field("password", &self.password.as_ref().map(|_| "<set>"))
            .finish()
    }
}

/// The stored port: a `u16` other than 0, else [`DEFAULT_REMOTE_WS_PORT`].
pub fn parse_port(raw: Option<&str>) -> u16 {
    raw.and_then(|v| v.trim().parse::<u16>().ok())
        .filter(|&p| p != 0)
        .unwrap_or(DEFAULT_REMOTE_WS_PORT)
}

/// Read the settings: `remote_ws_enabled == "true"` enables; the password is
/// kept verbatim, an empty or whitespace-only one means no auth.
pub async fn load_remote_settings(pool: &SqlitePool) -> Result<RemoteSettings, sqlx::Error> {
    use crate::db::models::get_setting;
    let enabled = get_setting(pool, SETTING_REMOTE_WS_ENABLED)
        .await?
        .is_some_and(|v| v.trim() == "true");
    let port = parse_port(get_setting(pool, SETTING_REMOTE_WS_PORT).await?.as_deref());
    let password = get_setting(pool, SETTING_REMOTE_WS_PASSWORD)
        .await?
        .filter(|p| !p.trim().is_empty());
    Ok(RemoteSettings {
        enabled,
        port,
        password,
    })
}

/// The last request a client sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LastRequest {
    pub request_type: String,
    /// Unix time, ms.
    pub at_ms: i64,
}

/// The outcome of the last scene switch (`program_switch`: a facade press or,
/// #221 L4a, a dashboard cut).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoteCut {
    /// The scene pressed, clipped to 64 characters (a client-chosen string).
    /// #221 L4a, a dashboard cut: the playlist's catalog scene, "OBS manuál"
    /// for -1, the playlist id when its catalog names no scene.
    pub scene: String,
    /// `playlist` / `input` / `keep`.
    pub action: &'static str,
    /// The program source cut to (`-1` = "OBS manuál"), `null` when kept.
    pub source: Option<i64>,
    /// Why nothing was cut: `not_switched` (cg OBS refused or did not answer
    /// a manual scene), `input_inactive`, `persist_failed`, `catalog_failed`.
    pub reason: Option<&'static str>,
    /// The boundary the cut lands on (`GET /api/v1/program`'s own field).
    pub cut_boundary_100ns: Option<i64>,
    /// Unix time, ms.
    pub at_ms: i64,
    /// #221: what triggered the switch, `program` (`SetCurrentProgramScene`),
    /// `transition` (`TriggerStudioModeTransition`) or (L4a) `dashboard`
    /// (`POST /api/v1/program/cut`).
    pub via: &'static str,
    /// #221: cg OBS's answer to a manual scene's forward: `ok`,
    /// `error <code>` or `not_ready`; `null` when nothing went to cg OBS (a
    /// playlist, "OBS manuál" itself, a dashboard cut — B4 step 6 deleted the
    /// mirror).
    pub cg_forward: Option<String>,
}

/// #221: the last `SetCurrentSceneTransitionDuration` a client sent. It is
/// acknowledged, never applied: the program transition is the Settings value
/// (cg OBS runs a Cut, so the button's duration never had a visible effect).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TransitionDuration {
    pub ms: u32,
    pub applied: bool,
}

/// The `remote` block of `GET /api/v1/program`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoteStatus {
    /// From the STORED settings (a save shows at once).
    pub enabled: bool,
    pub port: u16,
    /// A password is set (the obs-websocket challenge auth is required).
    pub auth: bool,
    /// The listener is bound (the settings task applies a save within 5 s).
    pub listening: bool,
    /// Why the listener is not bound (e.g. the port is taken).
    pub error: Option<String>,
    /// Connected clients (WebSocket sessions). A session's slot of
    /// [`MAX_SESSIONS`] is freed just before it stops counting here, so for
    /// that instant this may read one over the cap.
    pub clients: usize,
    /// #221: handshakes refused with HTTP 503 because [`MAX_SESSIONS`]
    /// sessions were open, since startup.
    pub refused_over_cap: u64,
    /// Requests served since startup (batch entries counted one by one).
    pub requests: u64,
    pub last_request: Option<LastRequest>,
    pub last_remote_cut: Option<RemoteCut>,
    /// Request types clients asked for that the facade does not serve.
    pub unsupported_requests: Vec<String>,
    /// #221: the last transition duration a client sent (not applied).
    pub last_transition_duration: Option<TransitionDuration>,
    /// #221 L3: SP-program's scene name (the one resolver,
    /// `program_scene_name`): what `GetCurrentProgramScene` answers and
    /// `CurrentProgramSceneChanged` announced last; `null` while nothing is on
    /// program, and also while a playlist whose catalog names no scene (an
    /// empty or duplicate NDI output name) is on it.
    pub program_scene: Option<String>,
}

#[derive(Default)]
struct RemoteState {
    listening: bool,
    error: Option<String>,
    last_request: Option<LastRequest>,
    last_cut: Option<RemoteCut>,
    unsupported: BTreeSet<String>,
    last_transition_duration: Option<TransitionDuration>,
    /// #221: when a refusal over the session cap was last logged.
    last_refusal_log: Option<std::time::Instant>,
}

/// The remote control's telemetry, shared by the settings task, the sessions
/// and the API (reached through `ProgramBus::remote()`). Every method holds
/// its lock for µs only.
#[derive(Default)]
pub struct RemoteShared {
    clients: AtomicUsize,
    requests: AtomicU64,
    refused_over_cap: AtomicU64,
    state: Mutex<RemoteState>,
}

impl RemoteShared {
    fn state(&self) -> MutexGuard<'_, RemoteState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The `remote` block for the stored `settings`, with `on_air` (what
    /// `SP-program` has on air) named as `program_scene`.
    pub fn status(&self, settings: &RemoteSettings, on_air: &OnAir) -> RemoteStatus {
        let st = self.state();
        RemoteStatus {
            enabled: settings.enabled,
            port: settings.port,
            auth: settings.password.is_some(),
            listening: st.listening,
            error: st.error.clone(),
            clients: self.clients.load(Ordering::SeqCst),
            refused_over_cap: self.refused_over_cap.load(Ordering::SeqCst),
            requests: self.requests.load(Ordering::SeqCst),
            last_request: st.last_request.clone(),
            last_remote_cut: st.last_cut.clone(),
            unsupported_requests: st.unsupported.iter().cloned().collect(),
            last_transition_duration: st.last_transition_duration,
            program_scene: program_scene_name(on_air),
        }
    }

    /// Count a connected client until the returned guard drops.
    pub fn client_connected(self: &Arc<Self>) -> ClientGuard {
        self.clients.fetch_add(1, Ordering::SeqCst);
        ClientGuard(Arc::clone(self))
    }

    /// #221: count a handshake refused over [`MAX_SESSIONS`] at `now`; the
    /// count so far, and whether to log this one (at most once per
    /// [`REFUSAL_LOG_INTERVAL`]).
    pub fn note_refused_over_cap(&self, now: std::time::Instant) -> (u64, bool) {
        let refused = self.refused_over_cap.fetch_add(1, Ordering::SeqCst) + 1;
        let mut st = self.state();
        let log = st
            .last_refusal_log
            .is_none_or(|last| now.saturating_duration_since(last) >= REFUSAL_LOG_INTERVAL);
        if log {
            st.last_refusal_log = Some(now);
        }
        (refused, log)
    }

    /// Count one request and remember it as the last one.
    pub fn record_request(&self, request_type: &str) {
        self.requests.fetch_add(1, Ordering::SeqCst);
        self.state().last_request = Some(LastRequest {
            request_type: clip(request_type),
            at_ms: now_ms(),
        });
    }

    /// Remember an unsupported request type (at most
    /// [`MAX_UNSUPPORTED_LISTED`], clipped); `true` the first time (log once).
    pub fn note_unsupported(&self, request_type: &str) -> bool {
        let mut st = self.state();
        if st.unsupported.len() >= MAX_UNSUPPORTED_LISTED {
            return false;
        }
        st.unsupported.insert(clip(request_type))
    }

    /// Remember the outcome of a scene switch (`last_remote_cut`).
    pub fn record_cut(&self, cut: RemoteCut) {
        self.state().last_cut = Some(cut);
    }

    /// #221: remember a client's transition duration (acknowledged, not
    /// applied).
    pub fn record_transition_duration(&self, ms: u32) {
        self.state().last_transition_duration = Some(TransitionDuration { ms, applied: false });
    }

    fn set_listening(&self, listening: bool) {
        self.state().listening = listening;
    }

    /// Replace the listener error; `true` when it is a NEW error (log it once,
    /// not on every retry).
    fn set_error(&self, error: Option<String>) -> bool {
        let mut st = self.state();
        let new = is_new_error(st.error.as_deref(), error.as_deref());
        st.error = error;
        new
    }
}

/// An error worth logging: there is one, and it differs from the previous one.
pub(crate) fn is_new_error(previous: Option<&str>, current: Option<&str>) -> bool {
    current.is_some() && previous != current
}

/// A client-chosen string (a request type, a scene name) as stored and
/// logged: clipped to [`MAX_REQUEST_TYPE_CHARS`].
pub(crate) fn clip(text: &str) -> String {
    text.chars().take(MAX_REQUEST_TYPE_CHARS).collect()
}

/// Decrements the client count when a session ends.
pub struct ClientGuard(Arc<RemoteShared>);

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.clients.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Unix time in ms.
pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The facade's link to cg OBS: SongPlayer's existing OBS client — its command
/// channel (`None` when OBS is not configured) and its event broadcast.
#[derive(Clone)]
pub struct Upstream {
    cmd_tx: Option<mpsc::Sender<ObsCommand>>,
    events: broadcast::Sender<ObsEvent>,
    /// How long a call waits for cg OBS: [`UPSTREAM_TIMEOUT`] (longer only in
    /// tests, so a "never awaited" test cannot be passed by a timeout and a
    /// stalled test runner never runs a switch out of its time).
    timeout: Duration,
}

impl Upstream {
    pub fn new(
        cmd_tx: Option<mpsc::Sender<ObsCommand>>,
        events: broadcast::Sender<ObsEvent>,
    ) -> Self {
        Self {
            cmd_tx,
            events,
            timeout: UPSTREAM_TIMEOUT,
        }
    }

    /// This link with another call timeout (tests only, the integration
    /// tests included: production always uses [`UPSTREAM_TIMEOUT`]).
    #[doc(hidden)]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Forward one request to cg OBS; its op=7 `d` object, `None` when cg OBS
    /// is not configured, not connected or did not answer within the timeout
    /// ([`UPSTREAM_TIMEOUT`]).
    pub async fn request(&self, request_type: &str, request_data: Option<Value>) -> Option<Value> {
        let rx = self.enqueue(request_type, request_data)?;
        self.wait(rx).await
    }

    /// Hand one request to the OBS client without ever blocking on its queue
    /// (FIFO: it goes out after every call queued before it); the receiver of
    /// its op=7 `d`, `None` when there is no OBS client or its queue is full.
    fn enqueue(
        &self,
        request_type: &str,
        request_data: Option<Value>,
    ) -> Option<oneshot::Receiver<Option<Value>>> {
        let tx = self.cmd_tx.as_ref()?;
        let (reply, rx) = oneshot::channel();
        let call = RemoteCall::Request {
            request_type: request_type.to_string(),
            request_data,
            deadline: tokio::time::Instant::now() + self.timeout,
            reply,
        };
        if tx.try_send(ObsCommand::Remote(call)).is_err() {
            warn!("remote: the OBS client's command queue is full or closed");
            return None;
        }
        Some(rx)
    }

    /// Wait at most the timeout for an enqueued request's op=7 `d`. Dropping
    /// `rx` on a timeout makes the OBS side skip the call if it runs later
    /// (`reply.is_closed()`), and the OBS side writes an awaited switch only
    /// while its whole answer timeout is left of this wait (its `deadline`):
    /// a switch cg OBS answers within the OBS client's 2 s never lands after
    /// the requester was told "not ready".
    async fn wait(&self, rx: oneshot::Receiver<Option<Value>>) -> Option<Value> {
        tokio::time::timeout(self.timeout, rx).await.ok()?.ok()?
    }

    /// A new receiver of cg OBS's events (one per session).
    pub fn subscribe(&self) -> broadcast::Receiver<ObsEvent> {
        self.events.subscribe()
    }
}

/// Everything a session needs: the pool (the playlists, input settings, the
/// persisted program source), the program bus (which also orders the scene
/// switches), cg OBS, the password of this listener and (#221 L3) the
/// facade's own events.
pub struct Facade {
    pool: SqlitePool,
    bus: Arc<ProgramBus>,
    upstream: Upstream,
    password: Option<String>,
    /// [`IDENTIFY_TIMEOUT`] (shorter only in tests).
    identify_timeout: Duration,
    /// #221 L3: the events the facade emits itself (`studio_events`), to
    /// every session of this listener.
    events: broadcast::Sender<FacadeEvent>,
    /// #221 L3: how long a switch's `SceneTransitionEnded` waits for the
    /// window at most: [`TRANSITION_END_MAX_WAIT`] (10 minutes in tests, so a
    /// stalled test runner never ends a fade's wait early — review round 2).
    transition_end_max: Duration,
    /// #221: one permit per session, [`MAX_SESSIONS`] in all; a handshake
    /// that gets none is refused with HTTP 503.
    sessions: Arc<Semaphore>,
}

impl Facade {
    pub fn new(
        pool: SqlitePool,
        bus: Arc<ProgramBus>,
        upstream: Upstream,
        password: Option<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            bus,
            upstream,
            password,
            identify_timeout: IDENTIFY_TIMEOUT,
            events: broadcast::channel(FACADE_EVENTS_CAPACITY).0,
            transition_end_max: TRANSITION_END_MAX_WAIT,
            sessions: Arc::new(Semaphore::new(MAX_SESSIONS)),
        })
    }

    /// A facade with its own identify timeout (tests).
    #[cfg(test)]
    pub(crate) fn for_test(
        pool: SqlitePool,
        bus: Arc<ProgramBus>,
        upstream: Upstream,
        password: Option<String>,
        identify_timeout: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            bus,
            upstream,
            password,
            identify_timeout,
            events: broadcast::channel(FACADE_EVENTS_CAPACITY).0,
            transition_end_max: Duration::from_secs(600),
            sessions: Arc::new(Semaphore::new(MAX_SESSIONS)),
        })
    }

    fn shared(&self) -> &Arc<RemoteShared> {
        self.bus.remote()
    }
}

/// Accept clients until this future is dropped (the listener task is
/// aborted); every session lives in the `JoinSet` and is dropped with it, and
/// so does this listener's program feedback (#221 L3,
/// `studio_events::run_program_feedback`).
pub async fn serve(listener: TcpListener, facade: Arc<Facade>) {
    let mut sessions = JoinSet::new();
    let (on_air, last) = studio_events::subscribe_program(&facade.bus);
    let feedback = studio_events::run_program_feedback(on_air, last, facade.events.clone());
    sessions.spawn(feedback);
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    sessions.spawn(session::run(stream, peer, Arc::clone(&facade)));
                }
                Err(e) => {
                    warn!(%e, "remote: accepting a client failed");
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                }
            },
            Some(_) = sessions.join_next() => {}
        }
    }
}

/// What the settings task does with the listener this poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListenerPlan {
    /// Leave it as it is.
    Keep,
    /// Stop the running listener (disabled).
    Stop,
    /// (Re)bind with the wanted settings — a change, or a retry after a
    /// failed bind.
    Start,
}

/// `running` = the settings of the bound listener (`None` = none bound).
pub(crate) fn listener_plan(
    running: Option<&RemoteSettings>,
    wanted: &RemoteSettings,
) -> ListenerPlan {
    match running {
        Some(r) if r == wanted => ListenerPlan::Keep,
        _ if wanted.enabled => ListenerPlan::Start,
        Some(_) => ListenerPlan::Stop,
        None => ListenerPlan::Keep,
    }
}

/// A bound listener: its settings + the accept task.
struct Running {
    settings: RemoteSettings,
    task: JoinHandle<()>,
}

impl Running {
    /// Abort the accept task and wait until it is gone: its listening socket
    /// is closed (the port is free before a rebind) and its `JoinSet` is
    /// dropped, which aborts every session (not awaited).
    async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

/// Start the settings task (called once from `PlaybackEngine::start_program`).
#[cfg_attr(test, mutants::skip)] // orchestration glue; the task itself is tested
pub fn start_remote(
    pool: SqlitePool,
    bus: Arc<ProgramBus>,
    upstream: Upstream,
    shutdown: &broadcast::Sender<()>,
) {
    let rx = shutdown.subscribe();
    tokio::spawn(run_remote_config_task(
        pool,
        bus,
        upstream,
        rx,
        REMOTE_SETTINGS_POLL,
    ));
}

/// Re-read the settings every `poll` and apply them to the listener until
/// shutdown.
pub async fn run_remote_config_task(
    pool: SqlitePool,
    bus: Arc<ProgramBus>,
    upstream: Upstream,
    mut shutdown: broadcast::Receiver<()>,
    poll: Duration,
) {
    let mut running: Option<Running> = None;
    loop {
        match load_remote_settings(&pool).await {
            Ok(wanted) => running = apply(running, wanted, &pool, &bus, &upstream).await,
            Err(e) => warn!(%e, "remote: reading the settings failed"),
        }
        tokio::select! {
            _ = shutdown.recv() => break,
            _ = tokio::time::sleep(poll) => {}
        }
    }
    if let Some(r) = running {
        r.stop().await;
    }
    bus.remote().set_listening(false);
    info!("remote: settings task stopped");
}

async fn apply(
    running: Option<Running>,
    wanted: RemoteSettings,
    pool: &SqlitePool,
    bus: &Arc<ProgramBus>,
    upstream: &Upstream,
) -> Option<Running> {
    let shared = bus.remote();
    match listener_plan(running.as_ref().map(|r| &r.settings), &wanted) {
        ListenerPlan::Keep => {
            if !wanted.enabled {
                // Disabled after a failed bind: nothing to stop, drop the error.
                shared.set_error(None);
            }
            running
        }
        ListenerPlan::Stop => {
            if let Some(r) = running {
                r.stop().await;
            }
            shared.set_listening(false);
            shared.set_error(None);
            info!("remote: disabled — listener stopped");
            None
        }
        ListenerPlan::Start => {
            if let Some(r) = running {
                r.stop().await;
            }
            shared.set_listening(false);
            match TcpListener::bind((Ipv4Addr::UNSPECIFIED, wanted.port)).await {
                Ok(listener) => {
                    let password = wanted.password.clone();
                    let facade =
                        Facade::new(pool.clone(), Arc::clone(bus), upstream.clone(), password);
                    let task = tokio::spawn(serve(listener, facade));
                    shared.set_listening(true);
                    shared.set_error(None);
                    info!(
                        port = wanted.port,
                        auth = wanted.password.is_some(),
                        "remote: listening (obs-websocket 5 subset)"
                    );
                    Some(Running {
                        settings: wanted,
                        task,
                    })
                }
                Err(e) => {
                    let error = format!("binding port {} failed: {e}", wanted.port);
                    if shared.set_error(Some(error.clone())) {
                        warn!(
                            error,
                            "remote: the listener is not bound — retried every poll"
                        );
                    }
                    None
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
