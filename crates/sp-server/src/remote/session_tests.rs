//! #213 remote-control sessions over REAL sockets: a tokio-tungstenite client
//! against `remote::serve`, a real `ProgramBus` + SQLite pool (with the
//! playlists sp-fast = 7 and sp-slow = 3, #221: the switch reads them), and cg
//! OBS faked at the OBS client's command channel (`ObsCommand::Remote`) — the
//! facade's actual upstream seam. Every wait is bounded (`TIMEOUT`), never a
//! sleep. The rig is `pub(super)`: the #221 studio-mode tests
//! (`session_tests_studio.rs`) share it.
//! Wired via `#[cfg(test)] #[path = "session_tests.rs"] mod tests;`.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::error::{Error, ProtocolError};
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::obs::remote_call::RemoteCall;
use crate::obs::{ObsCommand, ObsEvent};
use crate::playback::program_bus::{ProgramBus, SETTING_PROGRAM_SOURCE};
use crate::remote::{Facade, IDENTIFY_TIMEOUT, UPSTREAM_TIMEOUT, Upstream, serve};

pub(super) const TIMEOUT: Duration = Duration::from_secs(10);
/// The obs-websocket spec's example password (the only password in tests).
pub(super) const SPEC_PASSWORD: &str = "supersecretpassword";
/// cg OBS's scenes in the fake: two playlist scenes and two manual scenes.
pub(super) const SCENES: [&str; 4] = ["sp-fast", "sp-slow", "Slido", "Trailer"];
/// SongPlayer's playlists in the rig: `(id, ndi_output_name)`.
pub(super) const PLAYLISTS: [(i64, &str); 2] = [(7, "SP-fast"), (3, "SP-slow")];

pub(super) type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A fake cg OBS's call log.
pub(super) type Calls = Arc<Mutex<Vec<String>>>;

fn scene_list() -> Value {
    json!({
        "currentProgramSceneName": "sp-slow",
        "currentProgramSceneUuid": "u-slow",
        "currentPreviewSceneName": null,
        "currentPreviewSceneUuid": null,
        "scenes": SCENES
            .iter()
            .enumerate()
            .map(|(i, s)| json!({ "sceneIndex": i, "sceneName": s, "sceneUuid": format!("u-{s}") }))
            .collect::<Vec<_>>(),
    })
}

/// The fake cg OBS behind the OBS client's command channel. It records every
/// request as `"<type> <sceneName>"` and switches to a scene it has
/// ([`SCENES`], else 600).
pub(super) fn spawn_fake_upstream() -> (mpsc::Sender<ObsCommand>, Calls) {
    let (tx, mut rx) = mpsc::channel::<ObsCommand>(16);
    let calls = Calls::default();
    let log = Arc::clone(&calls);
    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            let ObsCommand::Remote(RemoteCall::Request {
                request_type,
                request_data,
                reply,
                ..
            }) = cmd
            else {
                continue;
            };
            let scene = request_data
                .as_ref()
                .and_then(|d| d["sceneName"].as_str())
                .unwrap_or("")
                .to_string();
            log.lock().unwrap().push(format!("{request_type} {scene}"));
            let ok = |data: Value| {
                json!({
                    "requestType": request_type,
                    "requestStatus": { "result": true, "code": 100 },
                    "responseData": data,
                })
            };
            let d = match request_type.as_str() {
                "GetSceneList" => ok(scene_list()),
                "SetCurrentProgramScene" if SCENES.contains(&scene.as_str()) => {
                    json!({ "requestStatus": { "result": true, "code": 100 } })
                }
                "SetCurrentProgramScene" => json!({
                    "requestStatus": {
                        "result": false,
                        "code": 600,
                        "comment": "No source was found by the name of `Nope`.",
                    }
                }),
                _ => ok(json!({})),
            };
            let _ = reply.send(Some(d));
        }
    });
    (tx, calls)
}

pub(super) struct Rig {
    pub(super) addr: SocketAddr,
    pub(super) pool: SqlitePool,
    pub(super) bus: Arc<ProgramBus>,
    pub(super) events: broadcast::Sender<ObsEvent>,
    pub(super) calls: Calls,
}

impl Rig {
    pub(super) fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    pub(super) fn remote(&self) -> crate::remote::RemoteStatus {
        let settings = crate::remote::RemoteSettings::disabled();
        self.bus.remote().status(&settings, &self.bus.on_air_now())
    }
}

pub(super) async fn rig_with(password: Option<&str>, upstream: bool) -> Rig {
    rig_full(password, upstream, IDENTIFY_TIMEOUT).await
}

async fn rig_full(password: Option<&str>, upstream: bool, identify_timeout: Duration) -> Rig {
    let (cmd_tx, calls) = spawn_fake_upstream();
    let cmd_tx = upstream.then_some(cmd_tx);
    rig_on(cmd_tx, calls, password, identify_timeout, UPSTREAM_TIMEOUT).await
}

/// A facade on a real bus + pool (with [`PLAYLISTS`]) that reaches cg OBS
/// through `cmd_tx` and waits for it at most `upstream_timeout`.
pub(super) async fn rig_on(
    cmd_tx: Option<mpsc::Sender<ObsCommand>>,
    calls: Calls,
    password: Option<&str>,
    identify_timeout: Duration,
    upstream_timeout: Duration,
) -> Rig {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    for (id, ndi) in PLAYLISTS {
        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active)
             VALUES (?, ?, ?, ?, 1)",
        )
        .bind(id)
        .bind(format!("p{id}"))
        .bind(format!("https://youtube.com/playlist?list=p{id}"))
        .bind(ndi)
        .execute(&pool)
        .await
        .unwrap();
    }
    let bus = Arc::new(ProgramBus::new());
    let (events, _) = broadcast::channel(64);
    let upstream = Upstream::new(cmd_tx, events.clone()).with_timeout(upstream_timeout);
    let facade = Facade::for_test(
        pool.clone(),
        Arc::clone(&bus),
        upstream,
        password.map(str::to_string),
        identify_timeout,
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(listener, facade));
    Rig {
        addr,
        pool,
        bus,
        events,
        calls,
    }
}

pub(super) async fn rig() -> Rig {
    rig_with(None, true).await
}

pub(super) async fn connect(addr: SocketAddr) -> Client {
    let mut req = format!("ws://{addr}").into_client_request().unwrap();
    req.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static("obswebsocket.json"),
    );
    let (ws, resp) = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req))
        .await
        .expect("connect timed out")
        .expect("connect failed");
    let echoed = resp.headers().get("sec-websocket-protocol").unwrap();
    assert_eq!(echoed.to_str().unwrap(), "obswebsocket.json");
    ws
}

/// The next JSON text message (panics on a close or after `TIMEOUT`).
pub(super) async fn next_json(ws: &mut Client) -> Value {
    loop {
        let msg = tokio::time::timeout(TIMEOUT, ws.next())
            .await
            .expect("no message within the timeout")
            .expect("the stream ended")
            .expect("read failed");
        match msg {
            Message::Text(text) => return serde_json::from_str(&text).unwrap(),
            Message::Close(frame) => panic!("closed by the server: {frame:?}"),
            _ => {}
        }
    }
}

/// The close frame the server sends next: `(code, reason)`.
pub(super) async fn next_close(ws: &mut Client) -> (u16, String) {
    let msg = tokio::time::timeout(TIMEOUT, ws.next())
        .await
        .expect("no close within the timeout")
        .expect("the stream ended without a close frame")
        .expect("read failed");
    match msg {
        Message::Close(Some(frame)) => (u16::from(frame.code), frame.reason.as_str().to_string()),
        Message::Close(None) => panic!("close frame without a code"),
        other => panic!("expected a close, got {other:?}"),
    }
}

/// One WebSocket connect attempt (JSON subprotocol) bounded by `deadline`:
/// `None` when the server dropped the handshake — a short-deadline rig does
/// that, correctly, when a stall delays the handshake past its deadline.
async fn try_connect(addr: SocketAddr, deadline: tokio::time::Instant) -> Option<Client> {
    let mut req = format!("ws://{addr}").into_client_request().unwrap();
    req.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static("obswebsocket.json"),
    );
    match tokio::time::timeout_at(deadline, tokio_tungstenite::connect_async(req)).await {
        Ok(Ok((ws, _))) => Some(ws),
        // Only "the server dropped the handshake" is a retry; any other
        // handshake error (e.g. no subprotocol echo) is a real regression.
        Ok(Err(Error::Io(_) | Error::Protocol(ProtocolError::HandshakeIncomplete))) => None,
        Ok(Err(e)) => panic!("the handshake failed: {e}"),
        Err(_) => panic!("no handshake answer within the test timeout"),
    }
}

/// Connect until a handshake lands (short-deadline rigs), bounded by TIMEOUT.
async fn connect_in_time(addr: SocketAddr) -> Client {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no handshake landed before the rig's deadline"
        );
        if let Some(ws) = try_connect(addr, deadline).await {
            return ws;
        }
    }
}

/// Connect AND identify on a short-deadline rig, retrying an attempt the rig's
/// deadline closed first (correct code under a stall), bounded by TIMEOUT.
async fn identified_in_time(addr: SocketAddr) -> Client {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "never identified before the rig's deadline"
        );
        let mut ws = connect_in_time(addr).await;
        next_json(&mut ws).await; // Hello, always sent before any close
        let identify = json!({ "op": 1, "d": { "rpcVersion": 1, "eventSubscriptions": 0 } });
        let _ = ws.send(Message::Text(identify.to_string().into())).await;
        match close_or_message(&mut ws).await {
            Ok(msg) if msg["op"] == 2 => return ws,
            // 4007, or (Windows) a reset that discarded the close frame.
            Err(Some(4007) | None) => {}
            other => panic!("unexpected answer to Identify: {other:?}"),
        }
    }
}

/// The next message (`Ok`), or the end of the session (`Err` with the close
/// code, `None` when the connection just ended).
async fn close_or_message(ws: &mut Client) -> Result<Value, Option<u16>> {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let next = tokio::time::timeout_at(deadline, ws.next())
            .await
            .expect("nothing within the timeout");
        match next {
            Some(Ok(Message::Text(text))) => return Ok(serde_json::from_str(&text).unwrap()),
            Some(Ok(Message::Close(frame))) => return Err(frame.map(|f| u16::from(f.code))),
            Some(Ok(_)) => {}
            Some(Err(_)) | None => return Err(None),
        }
    }
}

/// Read until the session ends: the close code the server sent, `None` when
/// the connection just ended (or failed).
pub(super) async fn close_code_at_end(ws: &mut Client) -> Option<u16> {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let next = tokio::time::timeout_at(deadline, ws.next())
            .await
            .expect("the session did not end within the timeout");
        match next {
            Some(Ok(Message::Close(frame))) => return frame.map(|f| u16::from(f.code)),
            Some(Ok(_)) => {}
            Some(Err(_)) | None => return None,
        }
    }
}

pub(super) async fn send_json(ws: &mut Client, v: Value) {
    ws.send(Message::Text(v.to_string().into())).await.unwrap();
}

/// Read the Hello and identify (no auth) with `subscriptions`.
pub(super) async fn hello_identify(ws: &mut Client, subscriptions: u64) -> Value {
    let hello = next_json(ws).await;
    assert_eq!(hello["op"], 0);
    send_json(
        ws,
        json!({ "op": 1, "d": { "rpcVersion": 1, "eventSubscriptions": subscriptions } }),
    )
    .await;
    let identified = next_json(ws).await;
    assert_eq!(
        identified,
        json!({ "op": 2, "d": { "negotiatedRpcVersion": 1 } })
    );
    hello
}

/// Send one request and return its RequestResponse `d`.
pub(super) async fn request(ws: &mut Client, request_type: &str, data: Option<Value>) -> Value {
    let id = format!("req-{request_type}");
    let mut d = json!({ "requestType": request_type, "requestId": id });
    if let Some(data) = data {
        d["requestData"] = data;
    }
    send_json(ws, json!({ "op": 6, "d": d })).await;
    let resp = next_json(ws).await;
    assert_eq!(resp["op"], 7);
    assert_eq!(resp["d"]["requestType"], request_type);
    assert_eq!(resp["d"]["requestId"], id);
    resp["d"].clone()
}

pub(super) async fn press(ws: &mut Client, scene: &str) -> Value {
    request(
        ws,
        "SetCurrentProgramScene",
        Some(json!({ "sceneName": scene })),
    )
    .await
}

pub(super) async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while !cond() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

pub(super) async fn enable_input(pool: &SqlitePool, enabled: bool) {
    use crate::db::models::set_setting;
    let value = if enabled { "true" } else { "false" };
    set_setting(pool, "ndi_input_enabled", value).await.unwrap();
    set_setting(pool, "ndi_input_source", "CG-OBS (manual)")
        .await
        .unwrap();
}

pub(super) async fn persisted_source(pool: &SqlitePool) -> Option<String> {
    crate::db::models::get_setting(pool, SETTING_PROGRAM_SOURCE)
        .await
        .unwrap()
}

// ---- handshake -------------------------------------------------------------

#[tokio::test]
async fn hello_without_auth_then_identify_is_identified_and_counted() {
    let rig = rig().await;
    let mut ws = connect(rig.addr).await;
    let hello = hello_identify(&mut ws, 0x7FF).await;
    assert_eq!(hello["d"]["rpcVersion"], 1);
    assert_eq!(hello["d"]["obsWebSocketVersion"], "5.0.0");
    assert!(hello["d"].get("authentication").is_none());
    assert_eq!(rig.remote().clients, 1);
}

#[tokio::test]
async fn hello_with_auth_accepts_the_string_computed_from_its_challenge() {
    let rig = rig_with(Some(SPEC_PASSWORD), true).await;
    let mut ws = connect(rig.addr).await;
    let hello = next_json(&mut ws).await;
    let auth = &hello["d"]["authentication"];
    let challenge = auth["challenge"].as_str().unwrap();
    let salt = auth["salt"].as_str().unwrap();
    assert_eq!(challenge.len(), 44);
    assert_eq!(salt.len(), 44);
    let answer = crate::obs::compute_auth(SPEC_PASSWORD, challenge, salt);
    send_json(
        &mut ws,
        json!({ "op": 1, "d": { "rpcVersion": 1, "authentication": answer } }),
    )
    .await;
    assert_eq!(next_json(&mut ws).await["op"], 2);
    // Identified: requests are served now.
    let d = request(&mut ws, "GetStudioModeEnabled", None).await;
    assert_eq!(d["requestStatus"]["code"], 100);
}

#[tokio::test]
async fn a_missing_or_wrong_authentication_is_closed_4009() {
    let rig = rig_with(Some(SPEC_PASSWORD), true).await;
    let mut ws = connect(rig.addr).await;
    next_json(&mut ws).await;
    send_json(&mut ws, json!({ "op": 1, "d": { "rpcVersion": 1 } })).await;
    let (code, reason) = next_close(&mut ws).await;
    assert_eq!(code, 4009);
    assert!(
        reason.contains("missing an `authentication` string"),
        "{reason}"
    );

    let mut ws = connect(rig.addr).await;
    next_json(&mut ws).await;
    let wrong = crate::obs::compute_auth(SPEC_PASSWORD, "another", "challenge");
    send_json(
        &mut ws,
        json!({ "op": 1, "d": { "rpcVersion": 1, "authentication": wrong } }),
    )
    .await;
    assert_eq!(
        next_close(&mut ws).await,
        (4009, "Authentication failed.".to_string())
    );
}

#[tokio::test]
async fn protocol_violations_close_with_the_obs_codes() {
    let rig = rig().await;
    // A request before Identify.
    let mut ws = connect(rig.addr).await;
    next_json(&mut ws).await;
    send_json(
        &mut ws,
        json!({ "op": 6, "d": { "requestType": "GetVersion", "requestId": "r" } }),
    )
    .await;
    assert_eq!(next_close(&mut ws).await.0, 4007);
    // A second Identify.
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    send_json(&mut ws, json!({ "op": 1, "d": { "rpcVersion": 1 } })).await;
    assert_eq!(next_close(&mut ws).await.0, 4008);
    // An unsupported RPC version.
    let mut ws = connect(rig.addr).await;
    next_json(&mut ws).await;
    send_json(&mut ws, json!({ "op": 1, "d": { "rpcVersion": 2 } })).await;
    assert_eq!(next_close(&mut ws).await.0, 4010);
    // A binary frame (obswebsocket.json is text only).
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    ws.send(Message::Binary(vec![1u8, 2, 3].into()))
        .await
        .unwrap();
    assert_eq!(next_close(&mut ws).await.0, 4002);
    // Not JSON.
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    ws.send(Message::Text("{".into())).await.unwrap();
    assert_eq!(next_close(&mut ws).await.0, 4002);
}

#[tokio::test]
async fn a_client_that_never_identifies_is_closed_after_the_identify_timeout() {
    let rig = rig_full(None, true, Duration::from_millis(300)).await;
    let mut ws = connect_in_time(rig.addr).await;
    next_json(&mut ws).await;
    let (code, reason) = next_close(&mut ws).await;
    assert_eq!(code, 4007);
    assert_eq!(reason, "No `Identify` arrived in time.");
    wait_for("the session is gone", || rig.remote().clients == 0).await;
}

#[tokio::test]
async fn an_identified_client_outlives_the_identify_timeout() {
    // No wall-time window: a LATER client that never identifies is the
    // witness. Its deadline is after A's, so once it is closed (or its
    // handshake dropped), A's deadline has passed too — and A must still be
    // served. A's own attempts are retried if a stall lets the deadline win.
    let rig = rig_full(None, true, Duration::from_millis(500)).await;
    let mut a = identified_in_time(rig.addr).await;
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    if let Some(mut witness) = try_connect(rig.addr, deadline).await {
        next_json(&mut witness).await;
        assert_eq!(next_close(&mut witness).await.0, 4007);
    }
    let d = request(&mut a, "GetStudioModeEnabled", None).await;
    assert_eq!(d["requestStatus"]["code"], 100);
    wait_for("only the identified client is left", || {
        rig.remote().clients == 1
    })
    .await;
}

#[tokio::test]
async fn a_socket_that_never_does_the_websocket_handshake_is_dropped() {
    use tokio::io::AsyncReadExt;
    let rig = rig_full(None, true, Duration::from_millis(300)).await;
    let mut raw = TcpStream::connect(rig.addr).await.unwrap();
    let mut buf = [0u8; 16];
    // Correct code drops the socket (EOF or a reset); it can never time out.
    let read = tokio::time::timeout(TIMEOUT, raw.read(&mut buf))
        .await
        .expect("the idle socket was kept past the timeout");
    assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
    assert_eq!(rig.remote().clients, 0);
}

#[tokio::test]
async fn a_message_over_1_mib_ends_the_session_without_being_parsed() {
    let rig = rig().await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    // Not JSON: parsed, it would be closed 4002; refused by size, it never is.
    let big = "x".repeat(1_100_000);
    let _ = ws.send(Message::Text(big.into())).await;
    assert_ne!(close_code_at_end(&mut ws).await, Some(4002));
    wait_for("the session is gone", || rig.remote().clients == 0).await;
}

#[tokio::test]
async fn the_client_count_follows_connects_and_disconnects() {
    let rig = rig().await;
    let mut a = connect(rig.addr).await;
    hello_identify(&mut a, 0).await;
    let mut b = connect(rig.addr).await;
    hello_identify(&mut b, 0).await;
    assert_eq!(rig.remote().clients, 2);
    a.close(None).await.unwrap();
    wait_for("one client left", || rig.remote().clients == 1).await;
    drop(b);
    wait_for("no client left", || rig.remote().clients == 0).await;
}

// ---- requests --------------------------------------------------------------

#[tokio::test]
async fn get_version_and_studio_mode_are_answered_by_the_facade() {
    let rig = rig().await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    let d = request(&mut ws, "GetVersion", None).await;
    assert_eq!(d["requestStatus"], json!({ "result": true, "code": 100 }));
    assert_eq!(d["responseData"], crate::remote::protocol::version_data());
    let d = request(&mut ws, "GetStudioModeEnabled", None).await;
    // #221: ON, so Companion's `do_transition` sends its transition.
    assert_eq!(d["responseData"], json!({ "studioModeEnabled": true }));
    // Neither reached cg OBS.
    assert!(rig.calls().is_empty(), "{:?}", rig.calls());
    let st = rig.remote();
    assert_eq!(st.requests, 2);
    assert_eq!(
        st.last_request.unwrap().request_type,
        "GetStudioModeEnabled"
    );
}

/// The scene list is cg OBS's, forwarded; its program and preview scene are
/// SongPlayer's (#221 lane 2, ROZHODNUTÉ 6002459249: never cg OBS's own
/// program, `session_tests_feedback.rs`) — here nothing is on SP-program
/// and no preview is set, so both are null.
#[tokio::test]
async fn the_scene_list_is_forwarded_to_cg_obs_with_songplayer_s_program() {
    let rig = rig().await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    let d = request(&mut ws, "GetSceneList", None).await;
    assert_eq!(d["requestStatus"], json!({ "result": true, "code": 100 }));
    let mut expected = scene_list();
    expected["currentProgramSceneName"] = Value::Null;
    expected["currentProgramSceneUuid"] = Value::Null;
    assert_eq!(d["responseData"], expected);
    let d = request(
        &mut ws,
        "GetSceneItemList",
        Some(json!({ "sceneName": "sp-fast" })),
    )
    .await;
    assert_eq!(d["requestStatus"]["code"], 100);
    assert_eq!(
        rig.calls(),
        vec!["GetSceneList ", "GetSceneItemList sp-fast"]
    );
}

#[tokio::test]
async fn an_unsupported_request_gets_204_and_is_listed_once() {
    let rig = rig().await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    for _ in 0..2 {
        let d = request(&mut ws, "GetStats", None).await;
        assert_eq!(
            d["requestStatus"],
            json!({
                "result": false,
                "code": 204,
                "comment": "SongPlayer's remote control does not serve `GetStats` (obs-websocket subset, #213)",
            })
        );
        assert!(d.get("responseData").is_none());
    }
    request(&mut ws, "GetHotkeyList", None).await;
    let st = rig.remote();
    assert_eq!(st.unsupported_requests, vec!["GetHotkeyList", "GetStats"]);
    assert_eq!(st.requests, 3);
    assert!(rig.calls().is_empty());
}

#[tokio::test]
async fn a_batch_runs_in_order_and_halts_on_failure_only_when_asked() {
    let rig = rig().await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    let batch = |halt: bool| {
        json!({ "op": 8, "d": {
            "requestId": "batch-1",
            "haltOnFailure": halt,
            "executionType": 0,
            "requests": [
                { "requestType": "GetVersion", "requestId": "a" },
                { "requestType": "GetStats", "requestId": "b" },
                { "requestType": "GetStudioModeEnabled" },
                { "requestId": "d" },
            ],
        }})
    };
    send_json(&mut ws, batch(false)).await;
    let resp = next_json(&mut ws).await;
    assert_eq!(resp["op"], 9);
    assert_eq!(resp["d"]["requestId"], "batch-1");
    let results = resp["d"]["results"].as_array().unwrap();
    let codes: Vec<u64> = results
        .iter()
        .map(|r| r["requestStatus"]["code"].as_u64().unwrap())
        .collect();
    assert_eq!(codes, vec![100, 204, 100, 203]);
    assert_eq!(results[0]["requestId"], "a");
    assert_eq!(results[0]["requestType"], "GetVersion");
    assert_eq!(results[1]["requestId"], "b");
    assert!(results[2].get("requestId").is_none());
    assert_eq!(
        results[2]["responseData"],
        json!({ "studioModeEnabled": true })
    );
    assert_eq!(results[3]["requestId"], "d");

    send_json(&mut ws, batch(true)).await;
    let resp = next_json(&mut ws).await;
    let results = resp["d"]["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[1]["requestStatus"]["code"], 204);
}

#[tokio::test]
async fn without_cg_obs_forwarded_requests_are_not_ready_and_nothing_is_cut() {
    let rig = rig_with(None, false).await;
    enable_input(&rig.pool, true).await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    let not_ready = json!({
        "result": false,
        "code": 207,
        "comment": "cg OBS is not reachable from SongPlayer right now",
    });
    let d = request(&mut ws, "GetSceneList", None).await;
    assert_eq!(d["requestStatus"], not_ready);
    let d = press(&mut ws, "Slido").await;
    assert_eq!(d["requestStatus"], not_ready);
    assert_eq!(rig.bus.status().source, None);
    let cut = rig.remote().last_remote_cut.unwrap();
    assert_eq!(cut.action, "keep");
    assert_eq!(cut.reason, Some("not_switched"));
}

// ---- SetCurrentProgramScene → SP-program (#221: the one switch path) -------

/// `GET /api/v1/program`'s `remote.last_remote_cut`, as the API serializes it.
pub(super) fn last_cut_json(rig: &Rig) -> Value {
    serde_json::to_value(rig.remote()).unwrap()["last_remote_cut"].clone()
}

#[tokio::test]
async fn a_manual_scene_keeps_the_program_when_the_input_is_not_a_source() {
    let rig = rig().await;
    enable_input(&rig.pool, false).await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    press(&mut ws, "sp-fast").await;
    assert_eq!(rig.bus.status().source, Some(7));
    // cg OBS switched (success), SP-program stays on the playlist.
    let d = press(&mut ws, "Slido").await;
    assert_eq!(d["requestStatus"]["code"], 100);
    assert_eq!(rig.bus.status().source, Some(7));
    assert_eq!(persisted_source(&rig.pool).await.as_deref(), Some("7"));
    let cut = rig.remote().last_remote_cut.unwrap();
    assert_eq!(cut.scene, "Slido");
    assert_eq!(cut.action, "keep");
    assert_eq!(cut.source, None);
    assert_eq!(cut.reason, Some("input_inactive"));
    assert_eq!(last_cut_json(&rig)["cg_forward"], "ok");
}

#[tokio::test]
async fn an_unknown_scene_passes_cg_obs_error_through_and_keeps_the_program() {
    let rig = rig().await;
    enable_input(&rig.pool, true).await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    let d = press(&mut ws, "Nope").await;
    assert_eq!(d["requestStatus"]["result"], false);
    assert_eq!(d["requestStatus"]["code"], 600);
    assert_eq!(rig.bus.status().source, None);
    assert_eq!(persisted_source(&rig.pool).await, None);
    // Nothing but the forward reached cg OBS.
    assert_eq!(rig.calls(), vec!["SetCurrentProgramScene Nope"]);
    let cut = rig.remote().last_remote_cut.unwrap();
    assert_eq!((cut.action, cut.reason), ("keep", Some("not_switched")));
    assert_eq!(last_cut_json(&rig)["cg_forward"], "error 600");
}

#[tokio::test]
async fn a_long_scene_name_is_forwarded_whole_and_recorded_clipped() {
    let rig = rig().await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    let long = "S".repeat(100);
    let d = press(&mut ws, &long).await;
    assert_eq!(d["requestStatus"]["code"], 600);
    assert_eq!(rig.calls(), vec![format!("SetCurrentProgramScene {long}")]);
    let cut = rig.remote().last_remote_cut.unwrap();
    assert_eq!(cut.scene, "S".repeat(64));
    assert_eq!(cut.reason, Some("not_switched"));
}

#[tokio::test]
async fn a_press_without_a_scene_name_is_300_and_not_forwarded() {
    let rig = rig().await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    let d = request(
        &mut ws,
        "SetCurrentProgramScene",
        Some(json!({ "sceneUuid": "u-sp-fast" })),
    )
    .await;
    assert_eq!(d["requestStatus"]["code"], 300);
    assert_eq!(d["requestStatus"]["result"], false);
    assert!(rig.calls().is_empty());
    assert_eq!(rig.remote().last_remote_cut, None);
}

#[tokio::test]
async fn a_cut_that_cannot_be_persisted_is_205_cuts_nothing_and_tells_cg_obs_nothing() {
    let rig = rig().await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    // The playlists are readable, the program source cannot be written.
    sqlx::query("DROP TABLE settings")
        .execute(&rig.pool)
        .await
        .unwrap();
    let d = press(&mut ws, "sp-fast").await;
    assert_eq!(d["requestStatus"]["code"], 205);
    assert_eq!(d["requestStatus"]["result"], false);
    assert_eq!(rig.bus.status().source, None);
    let cut = rig.remote().last_remote_cut.unwrap();
    assert_eq!(cut.action, "keep");
    assert_eq!(cut.source, None);
    assert_eq!(cut.reason, Some("persist_failed"));
    assert!(
        rig.calls().is_empty(),
        "cg OBS is told nothing: {:?}",
        rig.calls()
    );
}

// ---- events ----------------------------------------------------------------

fn raw(event_type: &str, data: Value) -> ObsEvent {
    ObsEvent::Raw {
        event_type: event_type.to_string(),
        event_data: data,
    }
}

#[tokio::test]
async fn cg_obs_scene_list_fans_out_but_its_program_scene_event_never_does() {
    let rig = rig().await;
    let mut a = connect(rig.addr).await;
    hello_identify(&mut a, 0x7FF).await;
    let mut b = connect(rig.addr).await;
    hello_identify(&mut b, 4).await;
    // Subscribed to General only: gets no scene event.
    let mut general = connect(rig.addr).await;
    hello_identify(&mut general, 1).await;

    // Not re-emitted: not a scene event, and (#221 L3) cg OBS's program
    // scene — the program feedback is SongPlayer's own.
    rig.events
        .send(raw("StreamStateChanged", json!({ "outputActive": true })))
        .unwrap();
    rig.events
        .send(raw(
            "CurrentProgramSceneChanged",
            json!({ "sceneName": "Slido", "sceneUuid": "u-Slido" }),
        ))
        .unwrap();
    // The scene list passes through: it is the first event either client gets.
    let scenes = json!({ "scenes": [{ "sceneName": "sp-fast" }] });
    rig.events
        .send(raw("SceneListChanged", scenes.clone()))
        .unwrap();
    let expected = json!({ "op": 5, "d": {
        "eventType": "SceneListChanged",
        "eventIntent": 4,
        "eventData": scenes,
    }});
    assert_eq!(next_json(&mut a).await, expected);
    assert_eq!(next_json(&mut b).await, expected);

    // The General-only client's next message is its own response, no event.
    let d = request(&mut general, "GetStudioModeEnabled", None).await;
    assert_eq!(d["requestStatus"]["code"], 100);
}

#[tokio::test]
async fn reidentify_changes_the_subscriptions() {
    let rig = rig().await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0x7FF).await;
    send_json(
        &mut ws,
        json!({ "op": 3, "d": { "eventSubscriptions": 0 } }),
    )
    .await;
    assert_eq!(next_json(&mut ws).await["op"], 2);
    // A Reidentify naming no subscriptions KEEPS them (obs-websocket's rule):
    // still 0, so the event is not delivered — the next message is the
    // response (`biased;` would deliver an already-queued event first).
    send_json(&mut ws, json!({ "op": 3, "d": {} })).await;
    assert_eq!(next_json(&mut ws).await["op"], 2);
    rig.events
        .send(raw("SceneListChanged", json!({ "scenes": ["a"] })))
        .unwrap();
    let d = request(&mut ws, "GetStudioModeEnabled", None).await;
    assert_eq!(d["requestStatus"]["code"], 100);

    send_json(
        &mut ws,
        json!({ "op": 3, "d": { "eventSubscriptions": 4 } }),
    )
    .await;
    assert_eq!(next_json(&mut ws).await["op"], 2);
    rig.events
        .send(raw("SceneListChanged", json!({ "scenes": ["b"] })))
        .unwrap();
    let ev = next_json(&mut ws).await;
    assert_eq!(ev["op"], 5);
    assert_eq!(ev["d"]["eventData"]["scenes"], json!(["b"]));

    // Kept at 4 too (a reset to 0 would drop this event).
    send_json(&mut ws, json!({ "op": 3, "d": {} })).await;
    assert_eq!(next_json(&mut ws).await["op"], 2);
    rig.events
        .send(raw("SceneListChanged", json!({ "scenes": ["c"] })))
        .unwrap();
    let ev = next_json(&mut ws).await;
    assert_eq!(ev["op"], 5);
    assert_eq!(ev["d"]["eventData"]["scenes"], json!(["c"]));
}
