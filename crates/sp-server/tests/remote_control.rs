//! #213 end to end: a Companion-like obs-websocket 5 client (real
//! tokio-tungstenite, `obswebsocket.json` subprotocol) → SongPlayer's remote
//! control (`remote::serve`) → SongPlayer's REAL OBS client
//! (`obs::ObsClient::spawn`, the one the facade reaches cg OBS through) → a
//! fake cg OBS (`FakeObsServer`). The program is a real `ProgramBus`.
//!
//! connect → Identify → GetSceneList (cg OBS's scenes 1:1) →
//! SetCurrentProgramScene(playlist scene) → cg OBS switched + `SP-program`
//! cut to the playlist → cg OBS's CurrentProgramSceneChanged re-emitted →
//! SetCurrentProgramScene(manual scene) → cut to "OBS manuál".
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

    // SongPlayer's remote control on a real program bus.
    let bus = Arc::new(ProgramBus::new());
    let facade = Facade::new(
        pool.clone(),
        Arc::clone(&bus),
        Upstream::new(Some(client.cmd_sender()), obs_event_tx.clone()),
        None,
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(remote::serve(listener, facade));

    // Companion's OBS module: connect with the JSON subprotocol and identify
    // with its subscriptions (All | InputActiveStateChanged | InputShowStateChanged).
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
    let studio = request(&mut ws, "GetStudioModeEnabled", None).await;
    assert_eq!(studio["responseData"]["studioModeEnabled"], false);

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
    assert_eq!(list["responseData"]["currentProgramSceneName"], "sp-slow");

    // A playlist scene button: cg OBS switches AND SP-program cuts to ytfast.
    let pressed = request(
        &mut ws,
        "SetCurrentProgramScene",
        Some(json!({ "sceneName": "sp-fast" })),
    )
    .await;
    assert_eq!(pressed["requestStatus"]["result"], true);
    assert_eq!(bus.status().source, Some(7));
    assert!(bus.status().cut_boundary_100ns.is_some_and(|b| b > 0));
    let cg_now = fake.state().await;
    assert_eq!(cg_now.program_scene.as_deref(), Some("sp-fast"));
    assert!(
        cg_now
            .requests
            .iter()
            .any(|r| r["requestType"] == "SetCurrentProgramScene"
                && r["requestData"]["sceneName"] == "sp-fast"),
        "cg OBS never got the switch: {:?}",
        cg_now.requests
    );
    assert_eq!(
        db::models::get_setting(&pool, "program_source")
            .await
            .unwrap()
            .as_deref(),
        Some("7")
    );

    // cg OBS's program-scene event reaches Companion (the button feedback).
    fake.push_event(
        "CurrentProgramSceneChanged",
        json!({ "sceneName": "sp-fast", "sceneUuid": "uuid-sp-fast" }),
    )
    .await;
    let event = loop {
        let msg = next_json(&mut ws).await;
        if msg["op"] == 5 {
            break msg;
        }
    };
    assert_eq!(
        event,
        json!({ "op": 5, "d": {
            "eventType": "CurrentProgramSceneChanged",
            "eventIntent": 4,
            "eventData": { "sceneName": "sp-fast", "sceneUuid": "uuid-sp-fast" },
        }})
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

    let remote = bus.remote().status(&remote::RemoteSettings::disabled());
    assert_eq!(remote.clients, 1);
    let cut = remote.last_remote_cut.unwrap();
    assert_eq!((cut.scene.as_str(), cut.action), ("Nope", "keep"));

    let _ = shutdown_tx.send(());
    fake.shutdown().await;
}
