//! #213 `remote/protocol.rs`: the obs-websocket 5 server wire format.
//! Wired via `#[cfg(test)] #[path = "protocol_tests.rs"] mod tests;`.

use serde_json::{Value, json};

use super::*;

/// The authentication example of the obs-websocket 5 spec
/// (`docs/generated/protocol.md`, "Creating an authentication string"); the
/// expected string was computed independently with Python `hashlib`.
const SPEC_PASSWORD: &str = "supersecretpassword";
const SPEC_CHALLENGE: &str = "+IxH4CnCiqpX1rM9scsNynZzbOe4KhDeYcTNS3PDaeY=";
const SPEC_SALT: &str = "lM1GncleQOaCu9lT1yeUZhFYnqhsLLP1G5lAGo3ixaI=";
const SPEC_AUTH: &str = "1Ct943GAT+6YQUUX47Ia/ncufilbe6+oD6lY+5kaCu4=";

fn spec_challenge() -> AuthChallenge {
    AuthChallenge {
        challenge: SPEC_CHALLENGE.to_string(),
        salt: SPEC_SALT.to_string(),
    }
}

#[test]
fn the_spec_auth_vector_is_accepted_and_nothing_else_is() {
    let c = spec_challenge();
    assert!(c.accepts(SPEC_PASSWORD, SPEC_AUTH));
    assert!(!c.accepts("supersecretpasswordX", SPEC_AUTH));
    assert!(!c.accepts(
        SPEC_PASSWORD,
        "1Ct943GAT+6YQUUX47Ia/ncufilbe6+oD6lY+5kaCu5="
    ));
    assert!(!c.accepts(SPEC_PASSWORD, ""));
    // Salt and challenge are not interchangeable.
    let swapped = AuthChallenge {
        challenge: SPEC_SALT.to_string(),
        salt: SPEC_CHALLENGE.to_string(),
    };
    assert!(!swapped.accepts(SPEC_PASSWORD, SPEC_AUTH));
}

#[test]
fn constant_time_eq_compares_length_and_every_byte() {
    assert!(constant_time_eq(b"abc", b"abc"));
    assert!(constant_time_eq(b"", b""));
    assert!(!constant_time_eq(b"abc", b"abd"));
    // Two differing bytes whose XORs are equal must not cancel out.
    assert!(!constant_time_eq(b"ab", b"ba"));
    assert!(!constant_time_eq(b"abc", b"ab"));
    assert!(!constant_time_eq(b"ab", b"abc"));
}

#[test]
fn a_random_challenge_is_32_bytes_of_base64_and_fresh_each_time() {
    use base64::Engine;
    let a = AuthChallenge::random();
    let b = AuthChallenge::random();
    let engine = base64::engine::general_purpose::STANDARD;
    assert_eq!(engine.decode(&a.challenge).unwrap().len(), 32);
    assert_eq!(engine.decode(&a.salt).unwrap().len(), 32);
    assert_ne!(a.challenge, a.salt);
    assert_ne!(a.challenge, b.challenge);
    assert_ne!(a.salt, b.salt);
}

#[test]
fn hello_without_auth_has_the_versions_and_no_authentication() {
    let h = hello(None);
    assert_eq!(h["op"], 0);
    assert_eq!(h["d"]["rpcVersion"], 1);
    assert_eq!(h["d"]["obsWebSocketVersion"], "5.0.0");
    assert_eq!(
        h["d"]["obsStudioVersion"],
        format!("SongPlayer {}", sp_core::config::VERSION)
    );
    assert!(h["d"].get("authentication").is_none());
}

#[test]
fn hello_with_auth_carries_the_challenge_and_salt() {
    let h = hello(Some(&spec_challenge()));
    assert_eq!(h["op"], 0);
    assert_eq!(h["d"]["authentication"]["challenge"], SPEC_CHALLENGE);
    assert_eq!(h["d"]["authentication"]["salt"], SPEC_SALT);
}

#[test]
fn identified_negotiates_rpc_version_1() {
    assert_eq!(
        identified(),
        json!({ "op": 2, "d": { "negotiatedRpcVersion": 1 } })
    );
}

#[test]
fn check_identify_accepts_the_right_auth_and_rpc_1() {
    let c = spec_challenge();
    assert_eq!(
        check_identify(1, Some(SPEC_AUTH), Some((&c, SPEC_PASSWORD))),
        Ok(())
    );
    // No password set: any (or no) authentication string is fine.
    assert_eq!(check_identify(1, None, None), Ok(()));
    assert_eq!(check_identify(1, Some("whatever"), None), Ok(()));
}

#[test]
fn check_identify_closes_4009_with_obs_wording_on_missing_or_wrong_auth() {
    let c = spec_challenge();
    let missing = check_identify(1, None, Some((&c, SPEC_PASSWORD))).unwrap_err();
    assert_eq!(missing.code, 4009);
    assert!(
        missing
            .reason
            .contains("missing an `authentication` string")
    );
    let wrong = check_identify(1, Some("bad"), Some((&c, SPEC_PASSWORD))).unwrap_err();
    assert_eq!(wrong.code, 4009);
    assert_eq!(wrong.reason, "Authentication failed.");
}

#[test]
fn check_identify_closes_4010_on_another_rpc_version_after_the_auth() {
    assert_eq!(check_identify(2, None, None).unwrap_err().code, 4010);
    assert_eq!(check_identify(0, None, None).unwrap_err().code, 4010);
    // Auth is checked first, like obs-websocket.
    let c = spec_challenge();
    let err = check_identify(2, Some("bad"), Some((&c, SPEC_PASSWORD))).unwrap_err();
    assert_eq!(err.code, 4009);
}

/// One JSON message parsed. `parse_client_message` takes the DECODED value
/// (#221 L2b: the session decodes the frame first); a frame that does not
/// decode is a session-level test (`session_tests.rs`).
fn parse(text: &str) -> Result<ClientMessage, CloseReason> {
    parse_client_message(serde_json::from_str(text).unwrap())
}

#[test]
fn subprotocol_negotiation_echoes_json_and_refuses_msgpack_only() {
    assert_eq!(negotiate_subprotocol(None), Subprotocol::Default);
    assert_eq!(negotiate_subprotocol(Some("  ")), Subprotocol::Default);
    assert_eq!(
        negotiate_subprotocol(Some("obswebsocket.json")),
        Subprotocol::Json
    );
    assert_eq!(
        negotiate_subprotocol(Some("obswebsocket.msgpack, obswebsocket.json")),
        Subprotocol::Json
    );
    assert_eq!(
        negotiate_subprotocol(Some("obswebsocket.msgpack")),
        Subprotocol::Unsupported
    );
}

#[test]
fn parse_identify_with_and_without_subscriptions() {
    let m = parse(r#"{"op":1,"d":{"rpcVersion":1,"authentication":"abc","eventSubscriptions":4}}"#)
        .unwrap();
    assert_eq!(
        m,
        ClientMessage::Identify {
            rpc_version: 1,
            authentication: Some("abc".to_string()),
            event_subscriptions: 4,
        }
    );
    let m = parse(r#"{"op":1,"d":{"rpcVersion":1}}"#).unwrap();
    assert_eq!(
        m,
        ClientMessage::Identify {
            rpc_version: 1,
            authentication: None,
            event_subscriptions: 0x7FF,
        }
    );
}

#[test]
fn parse_reidentify_keeps_the_subscriptions_when_none_are_named() {
    assert_eq!(
        parse(r#"{"op":3,"d":{"eventSubscriptions":65536}}"#).unwrap(),
        ClientMessage::Reidentify {
            event_subscriptions: Some(65536)
        }
    );
    assert_eq!(
        parse(r#"{"op":3,"d":{}}"#).unwrap(),
        ClientMessage::Reidentify {
            event_subscriptions: None
        }
    );
}

#[test]
fn parse_request_and_batch() {
    let m = parse(
        r#"{"op":6,"d":{"requestType":"SetCurrentProgramScene","requestId":"r1","requestData":{"sceneName":"sp-fast"}}}"#,
    )
    .unwrap();
    assert_eq!(
        m,
        ClientMessage::Request(RequestItem {
            request_type: Some("SetCurrentProgramScene".to_string()),
            request_id: Some("r1".to_string()),
            request_data: Some(json!({ "sceneName": "sp-fast" })),
        })
    );
    let m = parse(
        r#"{"op":8,"d":{"requestId":"b1","haltOnFailure":true,"executionType":0,"requests":[{"requestType":"GetVersion"},{"requestId":"x"}]}}"#,
    )
    .unwrap();
    assert_eq!(
        m,
        ClientMessage::Batch {
            request_id: "b1".to_string(),
            halt_on_failure: true,
            requests: vec![
                RequestItem {
                    request_type: Some("GetVersion".to_string()),
                    request_id: None,
                    request_data: None,
                },
                RequestItem {
                    request_type: None,
                    request_id: Some("x".to_string()),
                    request_data: None,
                },
            ],
        }
    );
    let m = parse(r#"{"op":8,"d":{"requestId":"b2","requests":[]}}"#).unwrap();
    assert_eq!(
        m,
        ClientMessage::Batch {
            request_id: "b2".to_string(),
            halt_on_failure: false,
            requests: vec![],
        }
    );
}

#[test]
fn malformed_messages_map_to_obs_close_codes() {
    let code = |text: &str| parse(text).unwrap_err().code;
    assert_eq!(code("[1,2]"), 4002);
    assert_eq!(code(r#"{"d":{}}"#), 4006);
    assert_eq!(code(r#"{"op":"6","d":{}}"#), 4006);
    assert_eq!(code(r#"{"op":4,"d":{}}"#), 4006);
    assert_eq!(code(r#"{"op":7,"d":{}}"#), 4006);
    assert_eq!(code(r#"{"op":1}"#), 4003);
    assert_eq!(code(r#"{"op":1,"d":[]}"#), 4003);
    assert_eq!(code(r#"{"op":1,"d":{}}"#), 4003);
    assert_eq!(code(r#"{"op":6,"d":{"requestId":"r"}}"#), 4003);
    assert_eq!(code(r#"{"op":6,"d":{"requestType":"GetVersion"}}"#), 4003);
    assert_eq!(code(r#"{"op":8,"d":{"requests":[]}}"#), 4003);
    assert_eq!(code(r#"{"op":8,"d":{"requestId":"b"}}"#), 4003);
    assert_eq!(decode_error().code, 4002);
    assert_eq!(NOT_IDENTIFIED.code, 4007);
    assert_eq!(IDENTIFY_TIMED_OUT.code, 4007);
    assert_eq!(ALREADY_IDENTIFIED.code, 4008);
}

#[test]
fn request_response_mirrors_type_and_id_with_status_and_data() {
    let r = request_response(
        "GetStudioModeEnabled",
        "id-7",
        &Reply::ok(Some(json!({ "studioModeEnabled": false }))),
    );
    assert_eq!(
        r,
        json!({
            "op": 7,
            "d": {
                "requestType": "GetStudioModeEnabled",
                "requestId": "id-7",
                "requestStatus": { "result": true, "code": 100 },
                "responseData": { "studioModeEnabled": false },
            }
        })
    );
    let e = request_response("GetStats", "id-8", &Reply::error(204, "nope"));
    assert_eq!(
        e,
        json!({
            "op": 7,
            "d": {
                "requestType": "GetStats",
                "requestId": "id-8",
                "requestStatus": { "result": false, "code": 204, "comment": "nope" },
            }
        })
    );
}

#[test]
fn batch_results_echo_optional_ids() {
    let with_id = RequestItem {
        request_type: Some("GetVersion".to_string()),
        request_id: Some("a".to_string()),
        request_data: None,
    };
    let r = batch_result(&with_id, &Reply::ok(Some(json!({ "x": 1 }))));
    assert_eq!(
        r,
        json!({
            "requestType": "GetVersion",
            "requestId": "a",
            "requestStatus": { "result": true, "code": 100 },
            "responseData": { "x": 1 },
        })
    );
    let no_id = RequestItem {
        request_type: None,
        request_id: None,
        request_data: None,
    };
    let r = batch_result(&no_id, &Reply::error(203, "m"));
    assert_eq!(
        r,
        json!({
            "requestType": "",
            "requestStatus": { "result": false, "code": 203, "comment": "m" },
        })
    );
    assert_eq!(
        batch_response("b", vec![json!(1)]),
        json!({ "op": 9, "d": { "requestId": "b", "results": [1] } })
    );
}

#[test]
fn reply_from_upstream_passes_cg_obs_status_and_data_through() {
    let d = json!({
        "requestType": "GetSceneList",
        "requestStatus": { "result": true, "code": 100 },
        "responseData": { "scenes": [{ "sceneName": "sp-fast" }] },
    });
    let r = Reply::from_upstream(&d);
    assert!(r.succeeded());
    assert_eq!(r.status, json!({ "result": true, "code": 100 }));
    assert_eq!(
        r.data,
        Some(json!({ "scenes": [{ "sceneName": "sp-fast" }] }))
    );

    let failed = Reply::from_upstream(&json!({
        "requestStatus": { "result": false, "code": 600, "comment": "No source was found" },
    }));
    assert!(!failed.succeeded());
    assert_eq!(failed.status["code"], 600);
    assert_eq!(failed.data, None);

    let broken = Reply::from_upstream(&json!({ "requestStatus": "x" }));
    assert!(!broken.succeeded());
    assert_eq!(broken.status["code"], 205);
    assert!(!Reply::from_upstream(&json!({})).succeeded());
}

#[test]
fn event_carries_type_intent_and_data() {
    let data = json!({ "sceneName": "sp-fast", "sceneUuid": "u-1" });
    assert_eq!(
        event("CurrentProgramSceneChanged", 4, &data),
        json!({
            "op": 5,
            "d": {
                "eventType": "CurrentProgramSceneChanged",
                "eventIntent": 4,
                "eventData": { "sceneName": "sp-fast", "sceneUuid": "u-1" },
            }
        })
    );
}

#[test]
fn only_cg_obs_scene_list_passes_through_on_the_scenes_intent() {
    assert_eq!(passthrough_intent("SceneListChanged"), Some(4));
    // #221 L3: cg OBS's program scene never — the program feedback is
    // SongPlayer's own (`studio_events`).
    assert_eq!(passthrough_intent("CurrentProgramSceneChanged"), None);
    assert_eq!(passthrough_intent("CurrentPreviewSceneChanged"), None);
    assert_eq!(passthrough_intent("StreamStateChanged"), None);
    assert_eq!(passthrough_intent(""), None);
    // #221: never cg OBS's studio-mode event — Companion would cache studio
    // mode OFF and every page-13 button would go silently dead.
    assert_eq!(passthrough_intent("StudioModeStateChanged"), None);
}

#[test]
fn subscribed_tests_the_intent_bit() {
    assert!(subscribed(0x7FF, 4));
    assert!(subscribed(4, 4));
    assert!(subscribed(4 | 1, 4));
    assert!(!subscribed(0, 4));
    assert!(!subscribed(1 | 2 | 8, 4));
}

#[test]
fn routes_of_the_companion_subset() {
    match route("GetVersion") {
        Route::Native(r) => {
            assert!(r.succeeded());
            assert_eq!(r.data, Some(version_data()));
        }
        other => panic!("GetVersion must be native, got {other:?}"),
    }
    // #221: studio mode ON, so Companion's `do_transition` sends its request.
    assert_eq!(
        route("GetStudioModeEnabled"),
        Route::Native(Reply::ok(Some(json!({ "studioModeEnabled": true }))))
    );
    assert_eq!(route("SetCurrentProgramScene"), Route::SetProgramScene);
    // Only the scene/input list getters go to cg OBS.
    assert_eq!(
        FORWARDED_REQUESTS.as_slice(),
        [
            "GetSceneList",
            "GetInputList",
            "GetSceneItemList",
            "GetGroupSceneItemList",
        ]
        .as_slice()
    );
    for t in FORWARDED_REQUESTS {
        assert_eq!(route(t), Route::Forward, "{t}");
    }
    // #221: the studio-mode requests of the page-13 buttons are served, and
    // (L3) the program scene is SP-program's own.
    for t in [
        "GetCurrentProgramScene",
        "SetCurrentPreviewScene",
        "GetCurrentPreviewScene",
        "TriggerStudioModeTransition",
        "SetCurrentSceneTransitionDuration",
    ] {
        assert_ne!(route(t), Route::Unsupported, "{t}");
        assert_ne!(route(t), Route::Forward, "{t} is never forwarded");
    }
    for t in [
        "GetStats",
        "GetHotkeyList",
        "SetStudioModeEnabled",
        "Sleep",
        "",
    ] {
        assert_eq!(route(t), Route::Unsupported, "{t}");
    }
}

#[test]
fn version_data_has_what_companion_reads_unguarded() {
    let v = version_data();
    assert_eq!(v["obsWebSocketVersion"], "5.0.0");
    assert_eq!(v["rpcVersion"], 1);
    assert_eq!(
        v["obsVersion"],
        format!("SongPlayer {}", sp_core::config::VERSION)
    );
    assert_eq!(v["supportedImageFormats"], json!([]));
    assert_eq!(v["platform"], "songplayer");
    let available: Vec<&str> = v["availableRequests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap())
        .collect();
    assert_eq!(
        available,
        vec![
            "GetVersion",
            "GetStudioModeEnabled",
            "SetCurrentProgramScene",
            "GetCurrentProgramScene",
            "SetCurrentPreviewScene",
            "GetCurrentPreviewScene",
            "TriggerStudioModeTransition",
            "SetCurrentSceneTransitionDuration",
            "GetSceneList",
            "GetInputList",
            "GetSceneItemList",
            "GetGroupSceneItemList",
        ]
    );
}

#[test]
fn the_unsupported_comment_names_the_request() {
    assert_eq!(
        unsupported_comment("GetStats"),
        "SongPlayer's remote control does not serve `GetStats` (obs-websocket subset, #213)"
    );
}

#[test]
fn reply_ok_and_error_shapes() {
    let ok = Reply::ok(None);
    assert!(ok.succeeded());
    assert_eq!(ok.status, json!({ "result": true, "code": 100 }));
    let err = Reply::error(207, "later");
    assert!(!err.succeeded());
    assert_eq!(
        err.status,
        json!({ "result": false, "code": 207, "comment": "later" })
    );
    let v: Value = err.status;
    assert_eq!(v["code"], STATUS_NOT_READY);
}

// ---- #221: studio mode -----------------------------------------------------

#[test]
fn the_studio_mode_requests_are_answered_by_the_session() {
    assert_eq!(route("SetCurrentPreviewScene"), Route::SetPreviewScene);
    assert_eq!(route("GetCurrentPreviewScene"), Route::GetPreviewScene);
    assert_eq!(
        route("TriggerStudioModeTransition"),
        Route::TriggerTransition
    );
    assert_eq!(
        route("SetCurrentSceneTransitionDuration"),
        Route::SetTransitionDuration
    );
}

#[test]
fn a_scene_request_needs_its_scene_name() {
    let data = json!({ "sceneName": "sp-fast", "sceneUuid": "u" });
    assert_eq!(scene_name(Some(&data)), Ok("sp-fast".to_string()));
    for data in [
        None,
        Some(json!({})),
        Some(json!({ "sceneUuid": "u-sp-fast" })),
        Some(json!({ "sceneName": 7 })),
    ] {
        let err = scene_name(data.as_ref()).unwrap_err();
        assert!(!err.succeeded(), "{data:?}");
        assert_eq!(err.status["code"], STATUS_MISSING_REQUEST_FIELD, "{data:?}");
    }
}

#[test]
fn the_preview_answer_and_the_no_scene_error() {
    assert_eq!(
        preview_scene_data("Slido"),
        json!({ "sceneName": "Slido", "currentPreviewSceneName": "Slido" })
    );
    let err = no_scene();
    assert!(!err.succeeded());
    assert_eq!(err.status["code"], STATUS_INVALID_RESOURCE_STATE);
    assert_eq!(STATUS_INVALID_RESOURCE_STATE, 604);
}

#[test]
fn a_transition_duration_is_validated_like_obs_websocket() {
    let data = |v: Value| Some(json!({ "transitionDuration": v }));
    let ok = |d: Option<Value>| transition_duration(d.as_ref()).unwrap();
    let code =
        |d: Option<Value>| transition_duration(d.as_ref()).unwrap_err().status["code"].clone();
    assert_eq!(ok(data(json!(2000))), 2000);
    assert_eq!(ok(data(json!(50))), TRANSITION_DURATION_MIN_MS);
    assert_eq!(ok(data(json!(20_000))), TRANSITION_DURATION_MAX_MS);
    assert_eq!(ok(data(json!(750.9))), 750, "a fraction is truncated");
    assert_eq!(code(None), STATUS_MISSING_REQUEST_FIELD);
    assert_eq!(code(Some(json!({}))), STATUS_MISSING_REQUEST_FIELD);
    assert_eq!(code(data(Value::Null)), STATUS_MISSING_REQUEST_FIELD);
    assert_eq!(code(data(json!("2000"))), STATUS_INVALID_REQUEST_FIELD_TYPE);
    assert_eq!(code(data(json!(49.99))), STATUS_REQUEST_FIELD_OUT_OF_RANGE);
    assert_eq!(
        code(data(json!(20_000.01))),
        STATUS_REQUEST_FIELD_OUT_OF_RANGE
    );
    assert_eq!(
        (
            STATUS_INVALID_REQUEST_FIELD_TYPE,
            STATUS_REQUEST_FIELD_OUT_OF_RANGE
        ),
        (401, 402)
    );
}

// ---- #221 L3: SP-program's own program scene -------------------------------

#[test]
fn get_current_program_scene_is_answered_from_sp_program() {
    assert_eq!(route("GetCurrentProgramScene"), Route::GetProgramScene);
    assert_eq!(
        program_scene_data("sp-fast"),
        json!({ "sceneName": "sp-fast", "currentProgramSceneName": "sp-fast" })
    );
    let nothing = nothing_on_program();
    assert!(!nothing.succeeded());
    assert_eq!(
        nothing.status,
        json!({ "result": false, "code": 604, "comment": "Nothing is on SP-program." })
    );
    assert_eq!(nothing.data, None);
    // `EventSubscription::Transitions` (bit 4).
    assert_eq!(EVENT_TRANSITIONS, 16);
}
