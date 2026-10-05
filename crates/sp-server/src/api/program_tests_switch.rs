//! #221 L4a: `POST /api/v1/program/cut` goes through the ONE switch path
//! (`program_switch::switch_source`, `via=dashboard`), through the real axum
//! router, with cg OBS faked at the OBS client's command channel (the link
//! `start_program` attaches to `legacy_cg`): a playlist is cut first and
//! mirrored, -1 is a cut only, the switch waits for the `switch_order`, and
//! `legacy_cg.shown` records what cg OBS accepted. Shares the helpers of
//! `program_tests.rs`.
//! Wired via `#[cfg(test)] #[path = "program_tests_switch.rs"] mod tests_switch;`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};

use super::tests::{add_playlist, call, enable_input};
use crate::AppState;
use crate::api::routes::tests::test_state;
use crate::obs::ObsCommand;
use crate::obs::remote_call::RemoteCall;
use crate::playback::program_bus::{ProgramBus, SETTING_PROGRAM_SOURCE, restore_selected_source};
use crate::remote::Upstream;

const TIMEOUT: Duration = Duration::from_secs(10);

/// A fake cg OBS's call log: `"<requestType> <sceneName>"`.
type Calls = Arc<Mutex<Vec<String>>>;

/// Attach a fake cg OBS to the dashboard's link: it logs every call and
/// accepts the scenes in `known`, refusing any other with 600.
fn attach_fake_cg(state: &AppState, known: &'static [&'static str]) -> Calls {
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
            let result = known.contains(&scene.as_str());
            let code = if result { 100 } else { 600 };
            let _ = reply.send(Some(
                json!({ "requestStatus": { "result": result, "code": code } }),
            ));
        }
    });
    let (events, _) = broadcast::channel(4);
    assert!(
        state
            .program_bus
            .legacy_cg()
            .attach(Upstream::new(Some(tx), events))
    );
    calls
}

fn calls_of(calls: &Calls) -> Vec<String> {
    calls.lock().unwrap().clone()
}

async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while !cond() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// `remote.last_remote_cut` as `GET /api/v1/program` serializes it.
fn last_cut(state: &AppState) -> Value {
    let settings = crate::remote::RemoteSettings::disabled();
    let status = state
        .program_bus
        .remote()
        .status(&settings, &state.program_bus.on_air_now());
    serde_json::to_value(status).unwrap()["last_remote_cut"].clone()
}

async fn cut(state: &AppState, source: i64) -> (StatusCode, Value) {
    let body = json!({ "source": source });
    call(state.clone(), "POST", "/api/v1/program/cut", Some(body)).await
}

#[tokio::test]
async fn a_dashboard_playlist_cut_is_cut_first_then_mirrored_to_cg_obs() {
    let state = test_state().await;
    let calls = attach_fake_cg(&state, &["sp-fast", "sp-slow"]);
    let fast = add_playlist(&state.pool, "fast").await; // NDI output SP-fast
    let (status, json) = cut(&state, fast).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["source"], fast);
    assert!(json["cut_boundary_100ns"].as_i64().is_some_and(|b| b > 0));
    assert_eq!(
        state.program_bus.on_air_now().scene.as_deref(),
        Some("sp-fast")
    );

    wait_for("cg OBS follows the cut", || {
        calls_of(&calls) == ["SetCurrentProgramScene sp-fast"]
    })
    .await;
    wait_for("the mirror's answer is recorded", || {
        last_cut(&state)["cg_forward"] == "ok"
    })
    .await;
    let record = last_cut(&state);
    assert_eq!(record["scene"], "sp-fast");
    assert_eq!(record["action"], "playlist");
    assert_eq!(record["source"], fast);
    assert_eq!(record["via"], "dashboard");
    assert_eq!(record["cut_boundary_100ns"], json["cut_boundary_100ns"]);

    wait_for("cg OBS shows the playlist", || {
        state.program_bus.legacy_cg().shown_now() == Some(fast)
    })
    .await;
    let (_, json) = call(state.clone(), "GET", "/api/v1/program", None).await;
    assert_eq!(json["legacy_cg"], json!({ "shown": fast }));
}

#[tokio::test]
async fn a_dashboard_cut_to_the_input_is_a_cut_only() {
    let state = test_state().await;
    let calls = attach_fake_cg(&state, &["sp-fast"]);
    let fast = add_playlist(&state.pool, "fast").await;
    cut(&state, fast).await;
    wait_for("cg OBS shows the playlist", || {
        state.program_bus.legacy_cg().shown_now() == Some(fast)
    })
    .await;

    enable_input(&state, "CG-OBS (manual)").await;
    let (status, json) = cut(&state, -1).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["source"], -1);
    let record = last_cut(&state);
    assert_eq!(record["scene"], "OBS manuál");
    assert_eq!(record["action"], "input");
    assert_eq!(record["source"], -1);
    assert_eq!(record["via"], "dashboard");
    assert_eq!(record["cg_forward"], Value::Null, "nothing went to cg OBS");
    assert_eq!(calls_of(&calls), ["SetCurrentProgramScene sp-fast"]);
    assert_eq!(
        json["legacy_cg"],
        json!({ "shown": fast }),
        "cg OBS keeps what it shows"
    );
}

#[tokio::test]
async fn a_refused_mirror_keeps_the_cut_and_changes_no_record() {
    let state = test_state().await;
    let calls = attach_fake_cg(&state, &[]);
    let fast = add_playlist(&state.pool, "fast").await;
    let (status, json) = cut(&state, fast).await;
    assert_eq!(status, StatusCode::OK, "cut first, never gated on cg OBS");
    assert_eq!(json["source"], fast);
    wait_for("the refusal is recorded", || {
        last_cut(&state)["cg_forward"] == "error 600"
    })
    .await;
    assert_eq!(calls_of(&calls), ["SetCurrentProgramScene sp-fast"]);
    assert_eq!(state.program_bus.legacy_cg().shown_now(), None);
}

/// #221 B4 step 6: a dashboard cut to a playlist sends NOTHING to cg OBS —
/// no mirror, so no `cg_forward` — and the program answer has no
/// `legacy_cg` record any more.
#[tokio::test]
async fn a_dashboard_playlist_cut_sends_nothing_to_cg_obs() {
    let state = test_state().await;
    let fast = add_playlist(&state.pool, "fast").await;
    let (status, json) = cut(&state, fast).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["source"], fast);
    let record = last_cut(&state);
    assert_eq!(record["scene"], "sp-fast");
    assert_eq!(record["action"], "playlist");
    assert_eq!(record["via"], "dashboard");
    assert_eq!(record["cg_forward"], Value::Null, "nothing went to cg OBS");
    assert!(json.get("legacy_cg").is_none(), "{json}");
}

/// A playlist whose catalog names no scene (here: inactive) is cut with no
/// scene, and cg OBS is not told.
#[tokio::test]
async fn a_playlist_that_names_no_scene_is_cut_without_telling_cg_obs() {
    let state = test_state().await;
    let calls = attach_fake_cg(&state, &["sp-fast"]);
    let fast = add_playlist(&state.pool, "fast").await;
    sqlx::query("UPDATE playlists SET is_active = 0 WHERE id = ?")
        .bind(fast)
        .execute(&state.pool)
        .await
        .unwrap();
    let (status, json) = cut(&state, fast).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["source"], fast);
    assert_eq!(state.program_bus.on_air_now().scene, None);
    let record = last_cut(&state);
    assert_eq!(record["scene"], fast.to_string());
    assert_eq!(record["action"], "playlist");
    assert_eq!(record["via"], "dashboard");
    assert_eq!(record["cg_forward"], Value::Null);
    assert!(calls_of(&calls).is_empty(), "{:?}", calls_of(&calls));
}

#[tokio::test]
async fn a_dashboard_cut_that_cannot_be_persisted_is_500_and_cuts_nothing() {
    let state = test_state().await;
    let calls = attach_fake_cg(&state, &["sp-fast"]);
    let fast = add_playlist(&state.pool, "fast").await;
    sqlx::query("DROP TABLE settings")
        .execute(&state.pool)
        .await
        .unwrap();
    let (status, _) = cut(&state, fast).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(state.program_bus.status().source, None);
    let record = last_cut(&state);
    assert_eq!(record["action"], "keep");
    assert_eq!(record["reason"], "persist_failed");
    assert_eq!(record["via"], "dashboard");
    assert!(calls_of(&calls).is_empty(), "nothing is mirrored");
}

/// The dashboard cut waits for the switch order like a press: while a switch
/// holds it, nothing is cut ("must not happen yet", the safe direction).
#[tokio::test]
async fn a_dashboard_cut_waits_for_the_switch_order() {
    let state = test_state().await;
    let fast = add_playlist(&state.pool, "fast").await;
    let order = state.program_bus.switch_order().lock().await;
    let cutting = tokio::spawn(cut_owned(state.clone(), fast));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(state.program_bus.status().source, None, "held by the order");
    assert!(!cutting.is_finished());
    drop(order);
    let (status, json) = tokio::time::timeout(TIMEOUT, cutting)
        .await
        .expect("the cut ran once the order was free")
        .unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["source"], fast);
}

async fn cut_owned(state: AppState, source: i64) -> (StatusCode, Value) {
    cut(&state, source).await
}

#[tokio::test]
async fn the_restored_playlist_is_what_cg_obs_was_last_told() {
    let state = test_state().await;
    let fast = add_playlist(&state.pool, "fast").await;
    let set = |value: String| {
        let pool = state.pool.clone();
        async move {
            crate::db::models::set_setting(&pool, SETTING_PROGRAM_SOURCE, &value)
                .await
                .unwrap();
        }
    };
    set(fast.to_string()).await;
    let bus = ProgramBus::new();
    assert_eq!(restore_selected_source(&state.pool, &bus).await, Some(fast));
    assert_eq!(bus.legacy_cg().shown_now(), Some(fast));

    // The NDI input names no playlist.
    enable_input(&state, "CG-OBS (manual)").await;
    set("-1".to_string()).await;
    let bus = ProgramBus::new();
    assert_eq!(restore_selected_source(&state.pool, &bus).await, Some(-1));
    assert_eq!(bus.legacy_cg().shown_now(), None);
}
