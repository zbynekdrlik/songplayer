//! #221 L2b: the facade speaks `obswebsocket.msgpack`, Companion's encoding
//! (obs-websocket-js in Node offers nothing else; the first cutover was
//! refused with HTTP 400, #221 comment 5881650057). Over REAL sockets with the
//! rig of `session_tests.rs`: the subprotocol offer picks the session's
//! encoding (JSON whenever offered, else msgpack), Companion's v3 sequence
//! runs over msgpack end to end — Hello, Identify, GetVersion, studio mode,
//! the page-13 pair — and cuts SP-program, every answer AND every event comes
//! as a binary msgpack frame, and a frame of the other encoding, an
//! undecodable frame and an oversized one end the session like obs-websocket.
//! The exact bytes (independent of rmp-serde) are pinned in `codec_tests.rs`.
//! Wired via `#[cfg(test)] #[path = "session_tests_msgpack.rs"] mod tests_msgpack;`.

use std::net::SocketAddr;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::error::Error;
use tokio_tungstenite::tungstenite::http::HeaderValue;

use super::tests::{
    Client, SPEC_PASSWORD, TIMEOUT, close_code_at_end, connect, hello_identify, last_cut_json,
    next_close, rig, rig_with, wait_for,
};
use crate::obs::ObsEvent;

/// A handshake offering `offer`: the echoed subprotocol and the session, or
/// the HTTP status of the refusal. The tungstenite client itself fails a
/// handshake whose echo it did not offer, or that echoes nothing for an offer.
async fn handshake(
    addr: SocketAddr,
    offer: Option<&'static str>,
) -> Result<(Option<String>, Client), u16> {
    let mut req = format!("ws://{addr}").into_client_request().unwrap();
    if let Some(offer) = offer {
        req.headers_mut()
            .insert("Sec-WebSocket-Protocol", HeaderValue::from_static(offer));
    }
    let answer = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req))
        .await
        .expect("no handshake answer within the timeout");
    match answer {
        Ok((ws, resp)) => {
            let echoed = resp
                .headers()
                .get("sec-websocket-protocol")
                .map(|v| v.to_str().unwrap().to_string());
            Ok((echoed, ws))
        }
        Err(Error::Http(resp)) => Err(resp.status().as_u16()),
        Err(e) => panic!("the handshake with {offer:?} failed: {e}"),
    }
}

/// Connect the way obs-websocket-js does in Node: offering only msgpack.
async fn connect_msgpack(addr: SocketAddr) -> Client {
    let (echoed, ws) = handshake(addr, Some("obswebsocket.msgpack"))
        .await
        .unwrap_or_else(|status| panic!("a msgpack client was refused: HTTP {status}"));
    assert_eq!(echoed.as_deref(), Some("obswebsocket.msgpack"));
    ws
}

/// Send one message as a msgpack client does: a binary frame.
async fn send_msgpack(ws: &mut Client, msg: &Value) {
    let frame = rmp_serde::to_vec_named(msg).unwrap();
    ws.send(Message::Binary(frame.into())).await.unwrap();
}

/// The next message, which must come in the session's encoding: a binary
/// msgpack frame (`binary`) or a JSON text frame. A frame of the other kind,
/// a close, or nothing within `TIMEOUT` fails the test.
async fn next_as(ws: &mut Client, binary: bool) -> Value {
    loop {
        let msg = tokio::time::timeout(TIMEOUT, ws.next())
            .await
            .expect("no message within the timeout")
            .expect("the stream ended")
            .expect("read failed");
        match msg {
            Message::Binary(frame) if binary => return rmp_serde::from_slice(&frame).unwrap(),
            Message::Text(text) if !binary => return serde_json::from_str(&text).unwrap(),
            other @ (Message::Binary(_) | Message::Text(_)) => {
                panic!("a frame of the other encoding (binary expected: {binary}): {other:?}")
            }
            Message::Close(frame) => panic!("closed by the server: {frame:?}"),
            _ => {} // ping / pong
        }
    }
}

async fn next_msgpack(ws: &mut Client) -> Value {
    next_as(ws, true).await
}

/// Read the msgpack Hello and identify with `subscriptions`: the Identified.
async fn identify_msgpack(ws: &mut Client, subscriptions: u64) -> Value {
    assert_eq!(next_msgpack(ws).await["op"], 0);
    let identify =
        json!({ "op": 1, "d": { "rpcVersion": 1, "eventSubscriptions": subscriptions } });
    send_msgpack(ws, &identify).await;
    next_msgpack(ws).await
}

/// One msgpack request as obs-websocket-js's `call` sends it (`requestData`
/// undefined → nil when there is none) → its RequestResponse `d`.
async fn request_msgpack(ws: &mut Client, request_type: &str, data: Value) -> Value {
    let id = format!("mp-{request_type}");
    let d = json!({ "requestId": id, "requestType": request_type, "requestData": data });
    send_msgpack(ws, &json!({ "op": 6, "d": d })).await;
    let resp = next_msgpack(ws).await;
    assert_eq!(resp["op"], 7, "{resp}");
    assert_eq!(resp["d"]["requestId"], id);
    assert_eq!(resp["d"]["requestType"], request_type);
    resp["d"].clone()
}

fn raw(event_type: &str, data: Value) -> ObsEvent {
    ObsEvent::Raw {
        event_type: event_type.to_string(),
        event_data: data,
    }
}

// ---- negotiation -------------------------------------------------------------

#[tokio::test]
async fn the_offered_subprotocol_picks_the_sessions_encoding() {
    let rig = rig().await;
    // (offer, echo, msgpack): JSON whenever offered (a JSON client is served
    // exactly as before), else msgpack — also next to an unknown protocol —
    // and JSON without a header when nothing is offered.
    let table: [(Option<&'static str>, Option<&str>, bool); 5] = [
        (Some("obswebsocket.json"), Some("obswebsocket.json"), false),
        (
            Some("obswebsocket.msgpack"),
            Some("obswebsocket.msgpack"),
            true,
        ),
        (
            Some("obswebsocket.msgpack, obswebsocket.json"),
            Some("obswebsocket.json"),
            false,
        ),
        (
            Some("chat, obswebsocket.msgpack"),
            Some("obswebsocket.msgpack"),
            true,
        ),
        (None, None, false),
    ];
    for (offer, echo, msgpack) in table {
        let (echoed, mut ws) = handshake(rig.addr, offer)
            .await
            .unwrap_or_else(|status| panic!("{offer:?} was refused: HTTP {status}"));
        assert_eq!(echoed.as_deref(), echo, "{offer:?}");
        // Every message of the session, both ways, is in that encoding: the
        // Hello, the Identify / Identified, a request and its response.
        assert_eq!(next_as(&mut ws, msgpack).await["op"], 0, "{offer:?}");
        let identify = json!({ "op": 1, "d": { "rpcVersion": 1, "eventSubscriptions": 0 } });
        let request = json!({ "op": 6, "d": {
            "requestType": "GetStudioModeEnabled",
            "requestId": "s",
        }});
        for msg in [&identify, &request] {
            if msgpack {
                send_msgpack(&mut ws, msg).await;
            } else {
                ws.send(Message::Text(msg.to_string().into()))
                    .await
                    .unwrap();
            }
        }
        assert_eq!(next_as(&mut ws, msgpack).await["op"], 2, "{offer:?}");
        let resp = next_as(&mut ws, msgpack).await;
        assert_eq!(
            resp["d"]["responseData"],
            json!({ "studioModeEnabled": true }),
            "{offer:?}"
        );
    }
    // Neither of obs-websocket's two: refused at the handshake.
    assert_eq!(handshake(rig.addr, Some("chat")).await.err(), Some(400));
}

// ---- Companion's v3 sequence over msgpack --------------------------------------

#[tokio::test]
async fn companion_over_msgpack_presses_a_page_13_button_and_gets_every_event_as_msgpack() {
    let rig = rig().await;
    let mut ws = connect_msgpack(rig.addr).await;
    let identified = identify_msgpack(&mut ws, 0x7FF).await;
    assert_eq!(
        identified,
        json!({ "op": 2, "d": { "negotiatedRpcVersion": 1 } })
    );
    // What the module asks at connect; both must succeed.
    let d = request_msgpack(&mut ws, "GetVersion", Value::Null).await;
    assert_eq!(d["requestStatus"], json!({ "result": true, "code": 100 }));
    assert_eq!(d["responseData"], crate::remote::protocol::version_data());
    assert!(d["responseData"]["supportedImageFormats"].is_array());
    let d = request_msgpack(&mut ws, "GetStudioModeEnabled", Value::Null).await;
    assert_eq!(d["responseData"], json!({ "studioModeEnabled": true }));
    // The scene list is cg OBS's, forwarded (a nil `requestData` included);
    // its program scene is SP-program's (#221 lane 2): nothing on it yet.
    let d = request_msgpack(&mut ws, "GetSceneList", Value::Null).await;
    assert!(
        d["responseData"]["currentProgramSceneName"].is_null(),
        "{d}"
    );
    // cg OBS's four, and SongPlayer's Blank (#245).
    assert_eq!(d["responseData"]["scenes"].as_array().unwrap().len(), 5);

    // preview_scene(sp-slow): answered, then the preview event, as msgpack.
    let d = request_msgpack(
        &mut ws,
        "SetCurrentPreviewScene",
        json!({ "sceneName": "sp-slow" }),
    )
    .await;
    assert_eq!(d["requestStatus"]["code"], 100);
    assert_eq!(
        next_msgpack(&mut ws).await,
        json!({ "op": 5, "d": {
            "eventType": "CurrentPreviewSceneChanged",
            "eventIntent": 4,
            "eventData": { "sceneName": "sp-slow" },
        }})
    );
    assert_eq!(rig.bus.status().source, None, "a preview cuts nothing");

    // do_transition: the cut happens — SP-program on sp-slow's playlist (3).
    let d = request_msgpack(&mut ws, "TriggerStudioModeTransition", Value::Null).await;
    assert_eq!(d["requestStatus"], json!({ "result": true, "code": 100 }));
    assert_eq!(rig.bus.status().source, Some(3));
    assert_eq!(rig.bus.on_air_now().scene.as_deref(), Some("sp-slow"));
    assert_eq!(last_cut_json(&rig)["via"], "transition");
    // SongPlayer's own feedback, all as msgpack: Started before Ended, the
    // program-scene event anywhere among them.
    let mut events = Vec::new();
    for _ in 0..3 {
        let msg = next_msgpack(&mut ws).await;
        assert_eq!(msg["op"], 5, "{msg}");
        events.push(msg["d"].clone());
    }
    let at = |event_type: &str| {
        events
            .iter()
            .position(|e| e["eventType"] == event_type)
            .unwrap_or_else(|| panic!("no {event_type} in {events:?}"))
    };
    assert!(at("SceneTransitionStarted") < at("SceneTransitionEnded"));
    assert_eq!(
        events[at("CurrentProgramSceneChanged")],
        json!({
            "eventType": "CurrentProgramSceneChanged",
            "eventIntent": 4,
            "eventData": { "sceneName": "sp-slow" },
        })
    );
    assert_eq!(
        events[at("SceneTransitionEnded")]["eventData"],
        json!({ "transitionName": "Cut" })
    );

    // cg OBS's scene list event passes through as msgpack too.
    let scenes = json!({ "scenes": [{ "sceneName": "sp-fast" }] });
    rig.events
        .send(raw("SceneListChanged", scenes.clone()))
        .unwrap();
    assert_eq!(
        next_msgpack(&mut ws).await,
        json!({ "op": 5, "d": {
            "eventType": "SceneListChanged",
            "eventIntent": 4,
            "eventData": scenes,
        }})
    );
    // #221 B4 step 6: the playlist press told cg OBS nothing. A forwarded
    // getter is the witness: the OBS client's queue is FIFO, so a switch the
    // press had queued would have reached the fake first.
    let d = request_msgpack(&mut ws, "GetInputList", Value::Null).await;
    assert_eq!(d["requestStatus"]["code"], 100);
    assert_eq!(rig.calls(), ["GetSceneList ", "GetInputList "]);

    // v4's batch (op 8 → op 9), as msgpack both ways.
    let batch = json!({ "op": 8, "d": {
        "requestId": "batch-mp",
        "haltOnFailure": false,
        "executionType": 0,
        "requests": [
            { "requestType": "GetStudioModeEnabled", "requestData": null },
            { "requestType": "GetCurrentPreviewScene", "requestId": "p" },
        ],
    }});
    send_msgpack(&mut ws, &batch).await;
    let resp = next_msgpack(&mut ws).await;
    assert_eq!(resp["op"], 9, "{resp}");
    assert_eq!(resp["d"]["requestId"], "batch-mp");
    let results = resp["d"]["results"].as_array().unwrap();
    assert_eq!(
        results[0]["responseData"],
        json!({ "studioModeEnabled": true })
    );
    assert_eq!(results[1]["requestId"], "p");
    assert_eq!(results[1]["responseData"]["sceneName"], "sp-slow");
    assert!(rig.remote().unsupported_requests.is_empty());
}

#[tokio::test]
async fn a_msgpack_client_authenticates_and_is_closed_4009_like_a_json_one() {
    let rig = rig_with(Some(SPEC_PASSWORD), true).await;
    let mut ws = connect_msgpack(rig.addr).await;
    let hello = next_msgpack(&mut ws).await;
    let auth = &hello["d"]["authentication"];
    let answer = crate::obs::compute_auth(
        SPEC_PASSWORD,
        auth["challenge"].as_str().unwrap(),
        auth["salt"].as_str().unwrap(),
    );
    let identify = json!({ "op": 1, "d": { "rpcVersion": 1, "authentication": answer } });
    send_msgpack(&mut ws, &identify).await;
    assert_eq!(next_msgpack(&mut ws).await["op"], 2);

    let mut ws = connect_msgpack(rig.addr).await;
    next_msgpack(&mut ws).await;
    let wrong = json!({ "op": 1, "d": { "rpcVersion": 1, "authentication": "wrong" } });
    send_msgpack(&mut ws, &wrong).await;
    assert_eq!(
        next_close(&mut ws).await,
        (4009, "Authentication failed.".to_string())
    );
}

// ---- frames a session refuses ------------------------------------------------

#[tokio::test]
async fn a_frame_of_the_other_encoding_closes_the_session_4002() {
    let rig = rig().await;
    // A text frame on a msgpack session — even valid JSON.
    let mut ws = connect_msgpack(rig.addr).await;
    identify_msgpack(&mut ws, 0).await;
    let request = json!({ "op": 6, "d": { "requestType": "GetVersion", "requestId": "r" } });
    ws.send(Message::Text(request.to_string().into()))
        .await
        .unwrap();
    assert_eq!(
        next_close(&mut ws).await,
        (
            4002,
            "Your session encoding is set to MsgPack, but a text message was received.".to_string()
        )
    );
    // A binary frame on a JSON session — even valid msgpack.
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    send_msgpack(&mut ws, &request).await;
    assert_eq!(
        next_close(&mut ws).await,
        (
            4002,
            "Your session encoding is set to Json, but a binary message was received.".to_string()
        )
    );
    wait_for("both sessions are gone", || rig.remote().clients == 0).await;
}

#[tokio::test]
async fn an_undecodable_or_non_object_msgpack_frame_is_4002() {
    let rig = rig().await;
    // Not msgpack (a reserved marker), before the Identify.
    let mut ws = connect_msgpack(rig.addr).await;
    next_msgpack(&mut ws).await;
    ws.send(Message::Binary(vec![0xc1_u8].into()))
        .await
        .unwrap();
    assert_eq!(
        next_close(&mut ws).await,
        (4002, "Unable to decode MsgPack.".to_string())
    );
    // Valid msgpack, but an array, not a message object.
    let mut ws = connect_msgpack(rig.addr).await;
    identify_msgpack(&mut ws, 0).await;
    send_msgpack(&mut ws, &json!([6, { "requestType": "GetVersion" }])).await;
    assert_eq!(
        next_close(&mut ws).await,
        (4002, "You sent a non-object payload.".to_string())
    );
}

#[tokio::test]
async fn a_binary_message_over_1_mib_ends_the_session_without_being_parsed() {
    let rig = rig().await;
    let mut ws = connect_msgpack(rig.addr).await;
    identify_msgpack(&mut ws, 0).await;
    // Not msgpack: parsed, it would be closed 4002; refused by size, never.
    let _ = ws
        .send(Message::Binary(vec![0xc1_u8; 1_100_000].into()))
        .await;
    assert_ne!(close_code_at_end(&mut ws).await, Some(4002));
    wait_for("the session is gone", || rig.remote().clients == 0).await;
}
