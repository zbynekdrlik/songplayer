//! #221 L2: the facade as a studio-mode switcher, over REAL sockets — the
//! page-13 Companion buttons (`SetCurrentPreviewScene` +
//! `TriggerStudioModeTransition`), the per-session preview, a playlist press
//! that never waits for cg OBS, the switch order across clients, and
//! `SetCurrentSceneTransitionDuration` (acknowledged, never applied). Shares
//! the rig of `session_tests.rs`.
//! Wired via `#[cfg(test)] #[path = "session_tests_studio.rs"] mod tests_studio;`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use super::tests::{
    Calls, Client, connect, enable_input, hello_identify, last_cut_json, next_json,
    persisted_source, press, request, rig, rig_on, rig_with, send_json, wait_for,
};
use super::tests_feedback::request_collecting;
use crate::obs::ObsCommand;
use crate::obs::remote_call::RemoteCall;
use crate::remote::IDENTIFY_TIMEOUT;

/// cg OBS's replies it holds back (open, unanswered) until the test answers.
type Held = Arc<Mutex<Vec<oneshot::Sender<Option<Value>>>>>;

/// A cg OBS that holds every `SetCurrentProgramScene` (the reply stays open
/// until the test sends it) and answers anything else at once.
fn spawn_holding_upstream() -> (mpsc::Sender<ObsCommand>, Calls, Held) {
    let (tx, mut rx) = mpsc::channel::<ObsCommand>(16);
    let calls = Calls::default();
    let held = Held::default();
    let (log, hold) = (Arc::clone(&calls), Arc::clone(&held));
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
            if request_type == "SetCurrentProgramScene" {
                hold.lock().unwrap().push(reply);
            } else {
                let _ = reply.send(Some(
                    json!({ "requestStatus": { "result": true, "code": 100 } }),
                ));
            }
        }
    });
    (tx, calls, held)
}

fn preview(scene: &str) -> Option<Value> {
    Some(json!({ "sceneName": scene }))
}

// ---- the one switch path (#221) ---------------------------------------------

/// The witness that a press told cg OBS nothing: a forwarded getter sent
/// after it. The OBS client's command queue is FIFO, so a switch the press
/// had queued would reach the fake before it.
async fn witness(ws: &mut Client) {
    let d = request(ws, "GetInputList", None).await;
    assert_eq!(d["requestStatus"]["code"], 100);
}

/// #221 B4 step 6: a playlist press cuts `SP-program` from SongPlayer's own
/// catalog and sends NOTHING to cg OBS (no mirror any more: cg OBS keeps
/// whatever manual scene it shows, only "OBS manuál" takes its program).
#[tokio::test]
async fn a_playlist_scene_is_cut_from_the_catalog_and_sends_nothing_to_cg_obs() {
    let rig = rig().await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    // Any ASCII case of the playlist's NDI output name is its scene.
    let d = press(&mut ws, "SP-Fast").await;
    assert_eq!(d["requestStatus"], json!({ "result": true, "code": 100 }));
    let status = rig.bus.status();
    assert_eq!(status.source, Some(7));
    assert!(status.cut_boundary_100ns.is_some_and(|b| b > 0));
    assert_eq!(persisted_source(&rig.pool).await.as_deref(), Some("7"));
    // Published under the catalog's name.
    assert_eq!(rig.bus.on_air_now().scene.as_deref(), Some("sp-fast"));
    witness(&mut ws).await;
    assert_eq!(rig.calls(), ["GetInputList "], "cg OBS is told nothing");
    let cut = last_cut_json(&rig);
    assert_eq!(cut["scene"], "SP-Fast");
    assert_eq!(cut["action"], "playlist");
    assert_eq!(cut["source"], 7);
    assert_eq!(cut["reason"], Value::Null);
    assert_eq!(cut["via"], "program");
    assert_eq!(cut["cg_forward"], Value::Null, "nothing went to cg OBS");
    assert_eq!(cut["cut_boundary_100ns"], json!(status.cut_boundary_100ns));
    assert!(cut["at_ms"].as_i64().unwrap() > 1_700_000_000_000, "{cut}");
}

#[tokio::test]
async fn a_manual_scene_cuts_to_obs_manual_after_cg_obs_switched_while_the_input_is_a_source() {
    let rig = rig().await;
    enable_input(&rig.pool, true).await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    press(&mut ws, "Slido").await;
    assert_eq!(rig.bus.status().source, Some(-1));
    assert_eq!(persisted_source(&rig.pool).await.as_deref(), Some("-1"));
    assert_eq!(rig.bus.on_air_now().scene.as_deref(), Some("Slido"));
    let cut = last_cut_json(&rig);
    assert_eq!(
        (&cut["action"], &cut["source"]),
        (&json!("input"), &json!(-1))
    );
    assert_eq!(cut["cg_forward"], "ok", "cg OBS answered before the cut");
    assert_eq!(rig.calls(), ["SetCurrentProgramScene Slido"]);

    press(&mut ws, "sp-slow").await;
    assert_eq!(rig.bus.status().source, Some(3));
    // Manual → manual: the input stays on air (no mix), the name changes.
    press(&mut ws, "Trailer").await;
    let cuts = rig.bus.status().health.cuts;
    press(&mut ws, "Slido").await;
    assert_eq!(rig.bus.status().source, Some(-1));
    assert_eq!(
        rig.bus.status().health.cuts,
        cuts,
        "no new cut: -1 stays on air"
    );
    assert_eq!(rig.bus.on_air_now().scene.as_deref(), Some("Slido"));
    assert_eq!(last_cut_json(&rig)["scene"], "Slido");
}

// ---- the page-13 buttons: preview + transition -----------------------------

#[tokio::test]
async fn a_companion_button_previews_then_transitions_and_cuts_the_program() {
    let rig = rig().await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0x7FF).await;
    // preview_scene(sp-fast): answered, the preview event follows, nothing cut.
    let d = request(&mut ws, "SetCurrentPreviewScene", preview("sp-fast")).await;
    assert_eq!(d["requestStatus"], json!({ "result": true, "code": 100 }));
    assert_eq!(
        next_json(&mut ws).await,
        json!({ "op": 5, "d": {
            "eventType": "CurrentPreviewSceneChanged",
            "eventIntent": 4,
            "eventData": { "sceneName": "sp-fast" },
        }})
    );
    assert_eq!(rig.bus.status().source, None, "a preview cuts nothing");
    assert!(rig.calls().is_empty(), "and asks cg OBS nothing");
    // do_transition: the preview goes on program.
    let d = request(&mut ws, "TriggerStudioModeTransition", None).await;
    assert_eq!(d["requestStatus"], json!({ "result": true, "code": 100 }));
    assert_eq!(rig.bus.status().source, Some(7));
    assert_eq!(rig.bus.on_air_now().scene.as_deref(), Some("sp-fast"));
    let cut = last_cut_json(&rig);
    assert_eq!(
        (cut["scene"].as_str(), cut["via"].as_str()),
        (Some("sp-fast"), Some("transition"))
    );
    assert!(rig.remote().unsupported_requests.is_empty());
    // #221 B4 step 6: cg OBS is told nothing. The transition's events come
    // first (this client subscribed to every intent), then the witness.
    let (d, _events) = request_collecting(&mut ws, "GetInputList", None).await;
    assert_eq!(d["requestStatus"]["code"], 100);
    assert_eq!(rig.calls(), ["GetInputList "], "cg OBS is told nothing");
}

#[tokio::test]
async fn the_preview_is_the_clients_own_else_the_program_scene() {
    let rig = rig().await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    // No preview and nothing on program: nothing to name, nothing to switch.
    for request_type in ["GetCurrentPreviewScene", "TriggerStudioModeTransition"] {
        let d = request(&mut ws, request_type, None).await;
        assert_eq!(d["requestStatus"]["code"], 604, "{request_type}");
        assert_eq!(d["requestStatus"]["result"], false);
    }
    assert_eq!(rig.bus.status().source, None);
    // Until the client sets one, the preview is the program scene now.
    rig.bus.select_initial(3, Some("sp-slow"));
    let d = request(&mut ws, "GetCurrentPreviewScene", None).await;
    assert_eq!(d["requestStatus"]["code"], 100);
    assert_eq!(
        d["responseData"],
        json!({ "sceneName": "sp-slow", "currentPreviewSceneName": "sp-slow" })
    );
    rig.bus.select_initial(-1, None);
    let d = request(&mut ws, "GetCurrentPreviewScene", None).await;
    assert_eq!(d["responseData"]["sceneName"], "OBS manuál");
    // Subscribed to nothing: no preview event (the next message is the
    // response of the next request).
    let d = request(&mut ws, "SetCurrentPreviewScene", preview("Slido")).await;
    assert_eq!(d["requestStatus"]["code"], 100);
    let d = request(&mut ws, "GetCurrentPreviewScene", None).await;
    assert_eq!(d["responseData"]["sceneName"], "Slido");
    // A request without a scene name changes nothing.
    let d = request(
        &mut ws,
        "SetCurrentPreviewScene",
        Some(json!({ "sceneUuid": "u-sp-fast" })),
    )
    .await;
    assert_eq!(d["requestStatus"]["code"], 300);
    let d = request(&mut ws, "GetCurrentPreviewScene", None).await;
    assert_eq!(d["responseData"]["sceneName"], "Slido");
}

#[tokio::test]
async fn each_client_keeps_its_own_preview() {
    let rig = rig().await;
    enable_input(&rig.pool, true).await;
    let mut companion = connect(rig.addr).await;
    hello_identify(&mut companion, 0x7FF).await;
    let mut driver = connect(rig.addr).await;
    hello_identify(&mut driver, 0x7FF).await;
    request(&mut companion, "SetCurrentPreviewScene", preview("sp-fast")).await;
    assert_eq!(
        next_json(&mut companion).await["d"]["eventData"]["sceneName"],
        "sp-fast"
    );
    request(&mut driver, "SetCurrentPreviewScene", preview("Slido")).await;
    assert_eq!(
        next_json(&mut driver).await["d"]["eventData"]["sceneName"],
        "Slido"
    );
    // Each transition takes its own client's preview. #221 L3: every cut is
    // fed back to BOTH clients (SongPlayer's program + transition events), so
    // the events before a response are collected and checked.
    request(&mut companion, "TriggerStudioModeTransition", None).await;
    assert_eq!(rig.bus.status().source, Some(7));
    let (_, before) = request_collecting(&mut driver, "TriggerStudioModeTransition", None).await;
    assert_eq!(rig.bus.status().source, Some(-1));
    // The companion never got the driver's preview event: the events before
    // its response are the program's own, and it still names its own preview.
    let (d, mut seen) = request_collecting(&mut companion, "GetCurrentPreviewScene", None).await;
    assert_eq!(d["responseData"]["sceneName"], "sp-fast");
    seen.extend(before);
    assert!(
        seen.iter()
            .all(|e| e["eventType"] != "CurrentPreviewSceneChanged"),
        "{seen:?}"
    );
}

#[tokio::test]
async fn a_transition_to_the_scene_on_air_always_switches() {
    let rig = rig().await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    press(&mut ws, "sp-fast").await;
    let seq = rig.bus.on_air_now().seq;
    let cuts = rig.bus.status().health.cuts;
    // No preview set: the preview is the program scene, sp-fast.
    let d = request(&mut ws, "TriggerStudioModeTransition", None).await;
    assert_eq!(d["requestStatus"]["code"], 100);
    assert_eq!(rig.bus.on_air_now().seq, seq + 1, "published again");
    assert_eq!(rig.bus.status().health.cuts, cuts, "a bus no-op");
    assert_eq!(last_cut_json(&rig)["via"], "transition");
    // A preview set to the scene on air switches the same way.
    request(&mut ws, "SetCurrentPreviewScene", preview("sp-fast")).await;
    request(&mut ws, "TriggerStudioModeTransition", None).await;
    assert_eq!(rig.bus.on_air_now().seq, seq + 2);
    assert_eq!(rig.bus.status().source, Some(7));
    // #221 B4 step 6: neither the press nor the re-kicks told cg OBS anything.
    witness(&mut ws).await;
    assert_eq!(rig.calls(), ["GetInputList "]);
}

#[tokio::test]
async fn a_transition_to_obs_manual_recuts_the_input_and_asks_cg_obs_nothing() {
    let rig = rig().await;
    enable_input(&rig.pool, true).await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    // "OBS manuál" restored at startup: no cg OBS scene is known for it.
    rig.bus.select_initial(-1, None);
    let d = request(&mut ws, "GetCurrentPreviewScene", None).await;
    assert_eq!(d["responseData"]["sceneName"], "OBS manuál");
    // A transition with no preview re-cuts the input itself.
    let d = request(&mut ws, "TriggerStudioModeTransition", None).await;
    assert_eq!(d["requestStatus"], json!({ "result": true, "code": 100 }));
    assert_eq!(rig.bus.status().source, Some(-1));
    let on_air = rig.bus.on_air_now();
    assert_eq!(
        (on_air.seq, on_air.scene.as_deref()),
        (2, None),
        "published again, still with no cg OBS scene"
    );
    assert!(
        rig.calls().is_empty(),
        "no scene to send to cg OBS: {:?}",
        rig.calls()
    );
    let cut = last_cut_json(&rig);
    assert_eq!(
        (
            cut["action"].as_str(),
            cut["source"].as_i64(),
            cut["via"].as_str()
        ),
        (Some("input"), Some(-1), Some("transition"))
    );
    assert_eq!(cut["cg_forward"], Value::Null);
    // With the input off, "OBS manuál" is not a source: kept.
    enable_input(&rig.pool, false).await;
    let d = press(&mut ws, "OBS manuál").await;
    assert_eq!(d["requestStatus"]["code"], 100);
    let cut = last_cut_json(&rig);
    assert_eq!(
        (cut["action"].as_str(), cut["reason"].as_str()),
        (Some("keep"), Some("input_inactive"))
    );
    assert!(rig.calls().is_empty(), "{:?}", rig.calls());
}

#[tokio::test]
async fn a_playlist_press_cuts_even_when_cg_obs_is_unreachable() {
    let rig = rig_with(None, false).await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    let d = press(&mut ws, "sp-slow").await;
    assert_eq!(d["requestStatus"], json!({ "result": true, "code": 100 }));
    assert_eq!(rig.bus.status().source, Some(3));
    let cut = last_cut_json(&rig);
    assert_eq!(cut["action"], "playlist");
    assert_eq!(
        cut["cg_forward"],
        Value::Null,
        "a playlist press asks cg OBS nothing"
    );
    // A manual press needs cg OBS: not ready, the program kept.
    let d = press(&mut ws, "Slido").await;
    assert_eq!(d["requestStatus"]["code"], 207);
    assert_eq!(rig.bus.status().source, Some(3));
    let cut = last_cut_json(&rig);
    assert_eq!(
        (
            cut["action"].as_str(),
            cut["reason"].as_str(),
            cut["cg_forward"].as_str()
        ),
        (Some("keep"), Some("not_switched"), Some("not_ready"))
    );
}

/// #221 B4 step 6: a playlist press never waits for cg OBS, because it sends
/// it nothing. cg OBS holds every switch here and the facade would wait 10
/// minutes for one, so a press that sent (and awaited) a switch could never
/// be answered in the test; both presses cut at once, and cg OBS holds
/// nothing.
#[tokio::test]
async fn a_playlist_press_never_waits_for_cg_obs() {
    let (cmd_tx, calls, held) = spawn_holding_upstream();
    let upstream_timeout = Duration::from_secs(600);
    let rig = rig_on(
        Some(cmd_tx),
        calls,
        None,
        IDENTIFY_TIMEOUT,
        upstream_timeout,
    )
    .await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    let d = press(&mut ws, "sp-fast").await;
    assert_eq!(d["requestStatus"]["code"], 100);
    assert_eq!(rig.bus.status().source, Some(7));
    let d = press(&mut ws, "sp-slow").await;
    assert_eq!(d["requestStatus"]["code"], 100);
    assert_eq!(rig.bus.status().source, Some(3));
    witness(&mut ws).await; // a getter: this cg OBS answers it at once
    assert_eq!(rig.calls(), ["GetInputList "]);
    assert!(held.lock().unwrap().is_empty(), "cg OBS holds no switch");
    let cut = last_cut_json(&rig);
    assert_eq!(
        (cut["scene"].as_str(), cut["cg_forward"].as_str()),
        (Some("sp-slow"), None)
    );
}

/// #221 review round 4: only a playlist press's mirror may supersede an
/// earlier switch in the OBS client's forwarder; a manual press's forward,
/// whose cut depends on cg OBS's answer, never does.
#[tokio::test]
async fn only_the_mirror_of_a_playlist_press_supersedes_an_earlier_switch() {
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<ObsCommand>(16);
    let seen: Arc<Mutex<Vec<(String, bool)>>> = Arc::default();
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            let ObsCommand::Remote(RemoteCall::Request {
                request_data,
                supersedes,
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
            log.lock().unwrap().push((scene, supersedes));
            let _ = reply.send(Some(
                json!({ "requestStatus": { "result": true, "code": 100 } }),
            ));
        }
    });
    let calls = Calls::default();
    let rig = rig_on(
        Some(cmd_tx),
        calls,
        None,
        IDENTIFY_TIMEOUT,
        Duration::from_secs(3),
    )
    .await;
    enable_input(&rig.pool, true).await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    press(&mut ws, "sp-fast").await;
    press(&mut ws, "Slido").await;
    wait_for("both switches reached cg OBS", || {
        seen.lock().unwrap().len() == 2
    })
    .await;
    assert_eq!(
        *seen.lock().unwrap(),
        [("sp-fast".to_string(), true), ("Slido".to_string(), false)],
        "the mirror supersedes, the manual forward does not"
    );
}

#[tokio::test]
async fn one_switch_at_a_time_across_every_client() {
    let rig = rig().await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    let order = rig.bus.switch_order().lock().await;
    send_json(
        &mut ws,
        json!({ "op": 6, "d": {
            "requestType": "SetCurrentProgramScene",
            "requestId": "waits",
            "requestData": { "sceneName": "sp-fast" },
        }}),
    )
    .await;
    // While another switch holds the order, this one cannot even cut. The
    // "not yet" window only errs in the safe direction.
    let early = tokio::time::timeout(Duration::from_millis(200), next_json(&mut ws)).await;
    assert!(
        early.is_err(),
        "a press ran while another switch held the order"
    );
    assert_eq!(rig.bus.status().source, None);
    drop(order);
    let answer = next_json(&mut ws).await;
    assert_eq!(answer["d"]["requestId"], "waits");
    assert_eq!(answer["d"]["requestStatus"]["code"], 100);
    assert_eq!(rig.bus.status().source, Some(7));
}

/// #221 review round 7: a manual press holds the switch order across its
/// awaited forward ("OBS manuál" carries cg OBS's program, so a later press
/// must not overtake it): another client's playlist press cannot even cut
/// until cg OBS answered the manual one.
#[tokio::test]
async fn a_manual_press_holds_the_switch_order_until_cg_obs_answers() {
    // cg OBS holds every switch; the facade would wait 10 minutes for it.
    let (cmd_tx, calls, held) = spawn_holding_upstream();
    let upstream_timeout = Duration::from_secs(600);
    let rig = rig_on(
        Some(cmd_tx),
        calls,
        None,
        IDENTIFY_TIMEOUT,
        upstream_timeout,
    )
    .await;
    enable_input(&rig.pool, true).await;
    let mut manual = connect(rig.addr).await;
    hello_identify(&mut manual, 0).await;
    let mut playlist = connect(rig.addr).await;
    hello_identify(&mut playlist, 0).await;
    let press_of = |id: &str, scene: &str| {
        json!({ "op": 6, "d": {
            "requestType": "SetCurrentProgramScene",
            "requestId": id,
            "requestData": { "sceneName": scene },
        }})
    };
    send_json(&mut manual, press_of("manual", "Slido")).await;
    wait_for("cg OBS holds the manual switch", || {
        held.lock().unwrap().len() == 1
    })
    .await;
    send_json(&mut playlist, press_of("playlist", "sp-fast")).await;
    // The "not yet" window only errs in the safe direction.
    let early = tokio::time::timeout(Duration::from_millis(200), next_json(&mut playlist)).await;
    assert!(
        early.is_err(),
        "a press overtook a manual press still waiting for cg OBS"
    );
    assert_eq!(rig.bus.status().source, None);
    let reply = held.lock().unwrap().pop().expect("the held manual switch");
    let _ = reply.send(Some(
        json!({ "requestStatus": { "result": true, "code": 100 } }),
    ));
    let answer = next_json(&mut manual).await;
    assert_eq!(answer["d"]["requestId"], "manual");
    assert_eq!(answer["d"]["requestStatus"]["code"], 100);
    let answer = next_json(&mut playlist).await;
    assert_eq!(answer["d"]["requestId"], "playlist");
    assert_eq!(answer["d"]["requestStatus"]["code"], 100);
    assert_eq!(rig.bus.status().source, Some(7));
    // #221 B4 step 6: only the manual press reached cg OBS.
    witness(&mut playlist).await;
    assert_eq!(
        rig.calls(),
        ["SetCurrentProgramScene Slido", "GetInputList "],
        "the playlist press tells cg OBS nothing"
    );
}

#[tokio::test]
async fn a_store_that_cannot_be_read_switches_nothing() {
    let rig = rig().await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    rig.pool.close().await;
    let d = press(&mut ws, "sp-fast").await;
    assert_eq!(d["requestStatus"]["code"], 205);
    assert_eq!(d["requestStatus"]["result"], false);
    assert_eq!(rig.bus.status().source, None);
    let cut = rig.remote().last_remote_cut.unwrap();
    assert_eq!((cut.action, cut.reason), ("keep", Some("catalog_failed")));
    assert_eq!(last_cut_json(&rig)["via"], "program");
    assert!(rig.calls().is_empty(), "{:?}", rig.calls());
}

#[tokio::test]
async fn a_batch_previews_then_transitions_in_order() {
    let rig = rig().await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0x7FF).await;
    send_json(
        &mut ws,
        json!({ "op": 8, "d": {
            "requestId": "button",
            "haltOnFailure": true,
            "requests": [
                { "requestType": "SetCurrentPreviewScene", "requestData": { "sceneName": "sp-slow" } },
                { "requestType": "TriggerStudioModeTransition" },
            ],
        }}),
    )
    .await;
    let resp = next_json(&mut ws).await;
    assert_eq!(resp["op"], 9);
    let codes: Vec<u64> = resp["d"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["requestStatus"]["code"].as_u64().unwrap())
        .collect();
    assert_eq!(codes, vec![100, 100]);
    assert_eq!(rig.bus.status().source, Some(3));
    // The preview event follows the batch's response.
    let event = next_json(&mut ws).await;
    assert_eq!(event["d"]["eventType"], "CurrentPreviewSceneChanged");
    assert_eq!(event["d"]["eventData"]["sceneName"], "sp-slow");
}

#[tokio::test]
async fn the_transition_duration_is_acknowledged_and_never_applied() {
    let rig = rig().await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    let before = rig.bus.status().transition;
    let duration = |ms: Value| Some(json!({ "transitionDuration": ms }));
    // The "ytfast" button [0/0] sends 2000 ms.
    let d = request(
        &mut ws,
        "SetCurrentSceneTransitionDuration",
        duration(json!(2000)),
    )
    .await;
    assert_eq!(d["requestStatus"], json!({ "result": true, "code": 100 }));
    assert_eq!(
        rig.bus.status().transition,
        before,
        "the program transition stays the Settings value"
    );
    let remote = serde_json::to_value(rig.remote()).unwrap();
    assert_eq!(
        remote["last_transition_duration"],
        json!({ "ms": 2000, "applied": false })
    );
    assert!(rig.calls().is_empty(), "nothing goes to cg OBS");
    // The bounds themselves are accepted.
    for ms in [50, 20_000] {
        let d = request(
            &mut ws,
            "SetCurrentSceneTransitionDuration",
            duration(json!(ms)),
        )
        .await;
        assert_eq!(d["requestStatus"]["code"], 100, "{ms}");
        let remote = serde_json::to_value(rig.remote()).unwrap();
        assert_eq!(remote["last_transition_duration"]["ms"], ms);
    }
    // Validated the way obs-websocket does; a refused one is not recorded.
    for (data, code) in [
        (None, 300),
        (Some(json!({})), 300),
        (duration(Value::Null), 300),
        (duration(json!("2000")), 401),
        (duration(json!(49)), 402),
        (duration(json!(49.9)), 402),
        (duration(json!(20_001)), 402),
        (duration(json!(20_000.5)), 402),
    ] {
        let d = request(&mut ws, "SetCurrentSceneTransitionDuration", data.clone()).await;
        assert_eq!(d["requestStatus"]["code"], code, "{data:?}");
        assert_eq!(d["requestStatus"]["result"], false, "{data:?}");
    }
    let remote = serde_json::to_value(rig.remote()).unwrap();
    assert_eq!(
        remote["last_transition_duration"],
        json!({ "ms": 20_000, "applied": false })
    );
    assert_eq!(rig.bus.status().transition, before);
}
