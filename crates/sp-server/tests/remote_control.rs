//! #213 end to end: an obs-websocket 5 client doing Companion's requests
//! (real tokio-tungstenite, over the `obswebsocket.json` subprotocol — Companion
//! itself speaks msgpack, #221 L2b, pinned by `remote/session_tests_msgpack.rs`)
//! → SongPlayer's remote
//! control (`remote::serve`) → SongPlayer's REAL OBS client
//! (`obs::ObsClient::spawn`, the one the facade reaches cg OBS through) → a
//! fake cg OBS (`FakeObsServer`). The program is a real `ProgramBus`.
//!
//! connect → Identify → studio mode ON (#221) → GetSceneList (cg OBS's scenes
//! 1:1) → a page-13 button, `SetCurrentPreviewScene(playlist scene)` +
//! `TriggerStudioModeTransition` → `SP-program` cut to the playlist from
//! SongPlayer's own playlists, cg OBS told nothing (#221 B4 step 6) →
//! Companion's feedback is
//! SongPlayer's OWN `CurrentProgramSceneChanged` (#221 L3), and cg OBS's is
//! never passed through → SetCurrentProgramScene(manual scene) → cg OBS
//! switched, then a cut to "OBS manuál".
//! Every wait is bounded; no sleep is used as synchronization.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use common::{FakeObsServer, FakeObsState};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sp_server::db;
use sp_server::obs;
use sp_server::playback::program_bus::ProgramBus;
use sp_server::remote::{self, Facade, Upstream};
use tokio::net::TcpListener;
use tokio::sync::{RwLock, broadcast};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

const TIMEOUT: Duration = Duration::from_secs(20);

type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn next_json(ws: &mut Client) -> Value {
    loop {
        let msg = tokio::time::timeout(TIMEOUT, ws.next())
            .await
            .expect("no message within the timeout")
            .expect("the stream ended")
            .expect("read failed");
        match msg {
            Message::Text(text) => return serde_json::from_str(&text).unwrap(),
            Message::Close(frame) => panic!("closed by SongPlayer: {frame:?}"),
            _ => {}
        }
    }
}

async fn send_json(ws: &mut Client, v: Value) {
    ws.send(Message::Text(v.to_string().into())).await.unwrap();
}

/// One request → its RequestResponse `d` (events in between are skipped).
async fn request(ws: &mut Client, request_type: &str, data: Option<Value>) -> Value {
    let id = format!("companion-{request_type}");
    let mut d = json!({ "requestType": request_type, "requestId": id });
    if let Some(data) = data {
        d["requestData"] = data;
    }
    send_json(ws, json!({ "op": 6, "d": d })).await;
    loop {
        let msg = next_json(ws).await;
        if msg["op"] == 7 && msg["d"]["requestId"] == id {
            return msg["d"].clone();
        }
    }
}

async fn wait_until<F, Fut>(what: &str, mut cond: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while !cond().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn companion_lists_cg_obs_scenes_and_a_scene_press_cuts_sp_program() {
    // SongPlayer's DB: the ytfast playlist (NDI output SP-fast, id 7) and the
    // #212 NDI input "OBS manuál" enabled with a source.
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active)
         VALUES (7, 'ytfast', 'https://youtube.com/playlist?list=PLfast', 'SP-fast', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    db::models::set_setting(&pool, "ndi_input_enabled", "true")
        .await
        .unwrap();
    db::models::set_setting(&pool, "ndi_input_source", "CG-OBS (manual)")
        .await
        .unwrap();

    // cg OBS: a playlist scene with SongPlayer's SP-fast NDI source, a baseline
    // scene, and a manual browser scene.
    let mut cg = FakeObsState::default();
    cg.inputs
        .insert("sp-fast_video".into(), "ndi_source".into());
    cg.input_settings.insert(
        "sp-fast_video".into(),
        json!({ "ndi_source_name": "RESOLUME-SNV (SP-fast)" }),
    );
    cg.scene_items.insert(
        "sp-fast".into(),
        vec![("sp-fast_video".into(), false, "ndi_source".into())],
    );
    cg.scene_items.insert(
        "Slido".into(),
        vec![("slido_browser".into(), false, "browser_source".into())],
    );
    cg.scene_list = vec!["sp-fast".into(), "sp-slow".into(), "Slido".into()];
    cg.program_scene = Some("sp-slow".into());
    let fake = FakeObsServer::spawn_with_state(cg).await;

    // SongPlayer's real OBS client, connected to cg OBS.
    let ndi_sources: obs::NdiSourceMap = Arc::new(RwLock::new(HashMap::new()));
    let obs_state = Arc::new(RwLock::new(obs::ObsState::default()));
    let (obs_event_tx, _) = broadcast::channel::<obs::ObsEvent>(64);
    let (_rebuild_tx, rebuild_rx) = broadcast::channel::<()>(4);
    let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(1);
    let client = obs::ObsClient::spawn(
        obs::ObsConfig {
            url: fake.url(),
            password: None,
        },
        pool.clone(),
        ndi_sources.clone(),
        obs_state.clone(),
        obs_event_tx.clone(),
        rebuild_rx,
        shutdown_rx,
    );
    wait_until("the OBS client maps sp-fast_video to playlist 7", || {
        let map = ndi_sources.clone();
        async move {
            let m = map.read().await;
            m.get("sp-fast_video") == Some(&7)
        }
    })
    .await;

    // SongPlayer's remote control on a real program bus. The long upstream
    // timeout keeps a stalled runner (the coverage job's ptrace) from running
    // a manual press out of its time (#221: then it is never sent).
    let bus = Arc::new(ProgramBus::new());
    let upstream =
        Upstream::new(Some(client.cmd_sender()), obs_event_tx.clone()).with_timeout(TIMEOUT);
    let facade = Facade::new(pool.clone(), Arc::clone(&bus), upstream, None);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(remote::serve(listener, facade));

    // Companion's requests over the JSON subprotocol (Companion itself speaks
    // msgpack, #221 L2b): identify with its subscriptions
    // (All | InputActiveStateChanged | InputShowStateChanged).
    let mut req = format!("ws://{addr}").into_client_request().unwrap();
    req.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static("obswebsocket.json"),
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let hello = next_json(&mut ws).await;
    assert_eq!(hello["op"], 0);
    assert_eq!(hello["d"]["rpcVersion"], 1);
    send_json(
        &mut ws,
        json!({ "op": 1, "d": { "rpcVersion": 1, "eventSubscriptions": 0x7FF | (1 << 17) | (1 << 18) } }),
    )
    .await;
    assert_eq!(next_json(&mut ws).await["op"], 2);

    // The capability checks Companion needs to stay connected.
    let version = request(&mut ws, "GetVersion", None).await;
    assert_eq!(version["requestStatus"]["code"], 100);
    assert!(version["responseData"]["supportedImageFormats"].is_array());
    // #221: studio mode ON — Companion's `do_transition` sends its request
    // only while it caches studio mode as on.
    let studio = request(&mut ws, "GetStudioModeEnabled", None).await;
    assert_eq!(studio["responseData"]["studioModeEnabled"], true);

    // The scene list is cg OBS's, 1:1.
    let list = request(&mut ws, "GetSceneList", None).await;
    assert_eq!(list["requestStatus"]["code"], 100);
    let names: Vec<&str> = list["responseData"]["scenes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["sceneName"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["sp-fast", "sp-slow", "Slido"]);
    // #221 lane 2: its program scene is SP-program's, never cg OBS's own
    // (sp-slow): nothing is on SP-program yet.
    assert!(
        list["responseData"]["currentProgramSceneName"].is_null(),
        "{list}"
    );

    // A page-13 playlist button: preview, then transition. SP-program cuts to
    // ytfast from SongPlayer's own playlists; #221 B4 step 6: cg OBS is told
    // nothing and keeps its own program.
    let previewed = request(
        &mut ws,
        "SetCurrentPreviewScene",
        Some(json!({ "sceneName": "sp-fast" })),
    )
    .await;
    assert_eq!(previewed["requestStatus"]["result"], true);
    assert_eq!(bus.status().source, None, "a preview cuts nothing");
    let pressed = request(&mut ws, "TriggerStudioModeTransition", None).await;
    assert_eq!(pressed["requestStatus"]["result"], true);
    assert_eq!(bus.status().source, Some(7));
    assert!(bus.status().cut_boundary_100ns.is_some_and(|b| b > 0));
    assert_eq!(
        db::models::get_setting(&pool, "program_source")
            .await
            .unwrap()
            .as_deref(),
        Some("7")
    );

    // #221 L3: Companion's button feedback is SongPlayer's OWN program.
    let feedback = loop {
        let msg = next_json(&mut ws).await;
        if msg["op"] == 5 && msg["d"]["eventType"] == "CurrentProgramSceneChanged" {
            break msg;
        }
    };
    assert_eq!(
        feedback,
        json!({ "op": 5, "d": {
            "eventType": "CurrentProgramSceneChanged",
            "eventIntent": 4,
            "eventData": { "sceneName": "sp-fast" },
        }})
    );
    // cg OBS's own program-scene event (a hand switch in its UI) is never
    // passed through; its scene list is, and it is the witness: pushed
    // after it on the same connection, it arrives after it.
    fake.push_event(
        "CurrentProgramSceneChanged",
        json!({ "sceneName": "Slido", "sceneUuid": "uuid-Slido" }),
    )
    .await;
    fake.push_event(
        "SceneListChanged",
        json!({ "scenes": [{ "sceneName": "sp-fast" }] }),
    )
    .await;
    let mut before = Vec::new();
    loop {
        let msg = next_json(&mut ws).await;
        if msg["op"] == 5 && msg["d"]["eventType"] == "SceneListChanged" {
            break;
        }
        before.push(msg);
    }
    assert!(
        before
            .iter()
            .all(|m| m["d"]["eventType"] != "CurrentProgramSceneChanged"),
        "cg OBS's program scene reached Companion: {before:?}"
    );
    // #221 B4 step 6: the playlist press told cg OBS nothing. The witness: a
    // getter forwarded after it (the OBS client writes the facade's calls in
    // queue order, so a switch the press had queued would have reached cg
    // OBS first). cg OBS keeps its own program.
    let inputs = request(&mut ws, "GetInputList", None).await;
    assert_eq!(inputs["requestStatus"]["code"], 100);
    let cg_now = fake.state().await;
    assert!(
        cg_now
            .requests
            .iter()
            .all(|r| r["requestType"] != "SetCurrentProgramScene"),
        "a playlist press switched cg OBS: {:?}",
        cg_now.requests
    );
    assert_eq!(cg_now.program_scene.as_deref(), Some("sp-slow"));
    // SongPlayer's program scene, never cg OBS's.
    let program = request(&mut ws, "GetCurrentProgramScene", None).await;
    assert_eq!(
        program["responseData"]["currentProgramSceneName"],
        "sp-fast"
    );
    // #221 lane 2: Companion's connect-time feedback (`GetSceneList`) names
    // it too, with cg OBS's uuid of that scene; cg OBS still shows sp-slow.
    let list = request(&mut ws, "GetSceneList", None).await;
    assert_eq!(list["responseData"]["currentProgramSceneName"], "sp-fast");
    assert_eq!(
        list["responseData"]["currentProgramSceneUuid"],
        "uuid-sp-fast"
    );

    // A manual cg OBS scene: cg OBS switches, SP-program cuts to "OBS manuál".
    let pressed = request(
        &mut ws,
        "SetCurrentProgramScene",
        Some(json!({ "sceneName": "Slido" })),
    )
    .await;
    assert_eq!(pressed["requestStatus"]["result"], true);
    assert_eq!(bus.status().source, Some(-1));
    assert_eq!(fake.state().await.program_scene.as_deref(), Some("Slido"));

    // An unknown scene: cg OBS refuses, the program stays on "OBS manuál".
    let pressed = request(
        &mut ws,
        "SetCurrentProgramScene",
        Some(json!({ "sceneName": "Nope" })),
    )
    .await;
    assert_eq!(pressed["requestStatus"]["code"], 600);
    assert_eq!(bus.status().source, Some(-1));

    let settings = remote::RemoteSettings::disabled();
    let remote = bus.remote().status(&settings, &bus.on_air_now());
    // #221 L3: the manual press put Slido on "OBS manuál"; the refused one
    // kept it.
    assert_eq!(remote.program_scene.as_deref(), Some("Slido"));
    assert_eq!(remote.clients, 1);
    let cut = remote.last_remote_cut.unwrap();
    assert_eq!((cut.scene.as_str(), cut.action), ("Nope", "keep"));

    let _ = shutdown_tx.send(());
    fake.shutdown().await;
}
