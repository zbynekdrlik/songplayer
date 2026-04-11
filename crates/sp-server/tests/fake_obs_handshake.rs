//! Smoke test that the shared `FakeObsServer` harness performs the OBS
//! WebSocket 5.x Hello/Identify/Identified handshake correctly.
//!
//! This is the foundation for every other scene-detection integration test.

mod common;

use common::{FakeObsServer, read_next_json};
use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn fake_obs_server_performs_identify_handshake() {
    let server = FakeObsServer::spawn().await;
    let url = server.url();

    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("client should connect to fake OBS");
    let (mut write, mut read) = ws.split();

    // Expect Hello (op 0) first.
    let hello = read_next_json(&mut read)
        .await
        .expect("should receive Hello");
    assert_eq!(hello["op"], 0, "first message must be Hello, got {hello}");
    assert_eq!(hello["d"]["rpcVersion"], 1);

    // Send Identify (op 1, no auth).
    let identify = json!({
        "op": 1,
        "d": { "rpcVersion": 1 }
    });
    write
        .send(Message::Text(identify.to_string().into()))
        .await
        .expect("send Identify");

    // Expect Identified (op 2) in response.
    let identified = read_next_json(&mut read)
        .await
        .expect("should receive Identified");
    assert_eq!(
        identified["op"], 2,
        "second message must be Identified, got {identified}"
    );
    assert_eq!(identified["d"]["negotiatedRpcVersion"], 1);

    server.shutdown().await;
}

#[tokio::test]
async fn fake_obs_server_responds_to_get_input_list() {
    let mut state = common::FakeObsState::default();
    state
        .inputs
        .insert("sp-fast_video".into(), "ndi_source".into());
    state.inputs.insert("Camera".into(), "dshow_input".into());

    let server = FakeObsServer::spawn_with_state(state).await;
    let url = server.url();

    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("client should connect");
    let (mut write, mut read) = ws.split();

    // Complete handshake.
    let _hello = read_next_json(&mut read).await.unwrap();
    write
        .send(Message::Text(
            json!({ "op": 1, "d": { "rpcVersion": 1 }})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let _identified = read_next_json(&mut read).await.unwrap();

    // Send GetInputList request.
    let request_id = "test-req-1";
    let request = json!({
        "op": 6,
        "d": {
            "requestType": "GetInputList",
            "requestId": request_id,
            "requestData": {}
        }
    });
    write
        .send(Message::Text(request.to_string().into()))
        .await
        .unwrap();

    // Read the response.
    let response = read_next_json(&mut read)
        .await
        .expect("should get response");
    assert_eq!(response["op"], 7);
    assert_eq!(response["d"]["requestId"], request_id);
    assert!(response["d"]["requestStatus"]["result"].as_bool().unwrap());

    let inputs = response["d"]["responseData"]["inputs"]
        .as_array()
        .expect("inputs array");
    assert_eq!(inputs.len(), 2);

    server.shutdown().await;
}

#[tokio::test]
async fn fake_obs_server_pushes_scene_change_events() {
    let server = FakeObsServer::spawn().await;
    let url = server.url();

    let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (mut write, mut read) = ws.split();

    // Complete handshake.
    let _hello = read_next_json(&mut read).await.unwrap();
    write
        .send(Message::Text(
            json!({ "op": 1, "d": { "rpcVersion": 1 }})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let _identified = read_next_json(&mut read).await.unwrap();

    // Push a scene change from the server side.
    server.push_program_scene_change("sp-fast").await;

    // Client should receive the event.
    let event = tokio::time::timeout(std::time::Duration::from_secs(1), read_next_json(&mut read))
        .await
        .expect("event within 1s")
        .expect("event text");
    assert_eq!(event["op"], 5);
    assert_eq!(event["d"]["eventType"], "CurrentProgramSceneChanged");
    assert_eq!(event["d"]["eventData"]["sceneName"], "sp-fast");

    server.shutdown().await;
}
