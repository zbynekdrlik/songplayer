//! #221 L4a: every facade press records what SongPlayer told cg OBS to show
//! (`playback::legacy_cg`), over REAL sockets with the rig of
//! `session_tests.rs`: an accepted mirror → its playlist, an accepted manual
//! scene → none, a refusal / no answer → unchanged, and a late answer to an
//! older press never overwrites a newer one's.
//! Wired via `#[cfg(test)] #[path = "session_tests_legacy.rs"] mod tests_legacy;`.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use super::tests::{
    Calls, Rig, TIMEOUT, connect, enable_input, hello_identify, last_cut_json, press, rig, rig_on,
    rig_with, wait_for,
};
use crate::obs::ObsCommand;
use crate::obs::remote_call::RemoteCall;
use crate::remote::IDENTIFY_TIMEOUT;

/// `legacy_cg.shown` now.
fn shown(rig: &Rig) -> Option<i64> {
    rig.bus.legacy_cg().shown_now()
}

#[tokio::test]
async fn an_accepted_mirror_shows_its_playlist_and_an_accepted_manual_scene_none() {
    let rig = rig().await;
    enable_input(&rig.pool, true).await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    assert_eq!(shown(&rig), None);

    press(&mut ws, "sp-fast").await;
    wait_for("the mirror is accepted", || {
        last_cut_json(&rig)["cg_forward"] == "ok"
    })
    .await;
    wait_for("cg OBS shows sp-fast", || shown(&rig) == Some(7)).await;

    press(&mut ws, "Slido").await;
    assert_eq!(last_cut_json(&rig)["cg_forward"], "ok");
    assert_eq!(shown(&rig), None, "cg OBS shows a manual scene");

    // With the NDI input off the press keeps SP-program, but cg OBS switched.
    press(&mut ws, "sp-slow").await;
    wait_for("cg OBS shows sp-slow", || shown(&rig) == Some(3)).await;
    enable_input(&rig.pool, false).await;
    press(&mut ws, "Trailer").await;
    assert_eq!(last_cut_json(&rig)["reason"], "input_inactive");
    assert_eq!(shown(&rig), None);
}

#[tokio::test]
async fn a_press_cg_obs_refuses_or_never_answers_changes_nothing() {
    let rig = rig().await;
    // A playlist whose scene cg OBS does not have: its mirror is refused.
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active)
         VALUES (9, 'p9', 'https://youtube.com/playlist?list=p9', 'SP-youth', 1)",
    )
    .execute(&rig.pool)
    .await
    .unwrap();
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    press(&mut ws, "sp-fast").await;
    wait_for("cg OBS shows sp-fast", || shown(&rig) == Some(7)).await;

    press(&mut ws, "sp-youth").await;
    assert_eq!(rig.bus.status().source, Some(9), "cut first, never gated");
    wait_for("the mirror is refused", || {
        last_cut_json(&rig)["cg_forward"] == "error 600"
    })
    .await;
    assert_eq!(shown(&rig), Some(7), "a refused mirror changes nothing");

    let d = press(&mut ws, "Nope").await;
    assert_eq!(d["requestStatus"]["code"], 600);
    assert_eq!(
        shown(&rig),
        Some(7),
        "a refused manual scene changes nothing"
    );
}

#[tokio::test]
async fn nothing_is_recorded_while_cg_obs_is_not_reachable() {
    let rig = rig_with(None, false).await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    press(&mut ws, "sp-fast").await;
    assert_eq!(rig.bus.status().source, Some(7));
    assert_eq!(last_cut_json(&rig)["cg_forward"], "not_ready");
    assert_eq!(shown(&rig), None);
}

/// cg OBS's replies it holds back until the test sends them.
type Held = Arc<Mutex<Vec<oneshot::Sender<Option<Value>>>>>;

/// A cg OBS that holds every `SetCurrentProgramScene` reply open.
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
            hold.lock().unwrap().push(reply);
        }
    });
    (tx, calls, held)
}

fn ok() -> Option<Value> {
    Some(json!({ "requestStatus": { "result": true, "code": 100 } }))
}

/// Two mirrors in flight, answered newest first: the late answer to the
/// OLDER press never overwrites the newer one's (each press took its ticket
/// under the switch order).
#[tokio::test]
async fn a_late_answer_to_an_older_press_never_overwrites_a_newer_one() {
    let (cmd_tx, calls, held) = spawn_holding_upstream();
    let rig = rig_on(Some(cmd_tx), calls, None, IDENTIFY_TIMEOUT, TIMEOUT * 60).await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    press(&mut ws, "sp-fast").await;
    press(&mut ws, "sp-slow").await;
    wait_for("both mirrors reach cg OBS", || {
        held.lock().unwrap().len() == 2
    })
    .await;
    assert_eq!(
        rig.calls(),
        [
            "SetCurrentProgramScene sp-fast",
            "SetCurrentProgramScene sp-slow"
        ]
    );
    let (older, newer) = {
        let mut held = held.lock().unwrap();
        let newer = held.pop().unwrap();
        (held.pop().unwrap(), newer)
    };
    newer.send(ok()).unwrap();
    wait_for("cg OBS shows sp-slow", || shown(&rig) == Some(3)).await;
    older.send(ok()).unwrap();
    // A "must not change" window, only in the safe direction: correct code
    // never changes `shown` here, whatever the runner's speed; a ticket taken
    // at the answer instead of the press would flip it to 7 within it.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(shown(&rig), Some(3), "the late answer is dropped");
    assert_eq!(last_cut_json(&rig)["scene"], "sp-slow");
}
