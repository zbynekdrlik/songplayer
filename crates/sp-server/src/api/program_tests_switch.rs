//! #221 L4a: `POST /api/v1/program/cut` goes through the ONE switch path
//! (`program_switch::switch_source`, `via=dashboard`), through the real axum
//! router: a playlist is cut with its catalog scene, -1 with none, the switch
//! waits for the `switch_order`, and (B4 step 6) cg OBS is told nothing — the
//! dashboard has no way to cg OBS at all any more. Shares the helpers of
//! `program_tests.rs`.
//! Wired via `#[cfg(test)] #[path = "program_tests_switch.rs"] mod tests_switch;`.

use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::tests::{add_playlist, call, enable_input};
use crate::AppState;
use crate::api::routes::tests::test_state;

const TIMEOUT: Duration = Duration::from_secs(10);

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

/// The cut lands on the catalog scene and is recorded with the cut's own
/// boundary.
#[tokio::test]
async fn a_dashboard_playlist_cut_is_published_and_recorded_with_its_catalog_scene() {
    let state = test_state().await;
    let fast = add_playlist(&state.pool, "fast").await; // ndi_output_name SP-fast
    let (status, json) = cut(&state, fast).await;
    assert_eq!(status, StatusCode::OK);
    assert!(json["cut_boundary_100ns"].as_i64().is_some_and(|b| b > 0));
    assert_eq!(
        state.program_bus.on_air_now().scene.as_deref(),
        Some("sp-fast")
    );
    let record = last_cut(&state);
    assert_eq!(record["source"], fast);
    assert_eq!(record["cut_boundary_100ns"], json["cut_boundary_100ns"]);
}

#[tokio::test]
async fn a_dashboard_cut_to_the_input_is_a_cut_only() {
    let state = test_state().await;
    let fast = add_playlist(&state.pool, "fast").await;
    cut(&state, fast).await;
    enable_input(&state, "CG-OBS (manual)").await;
    let (status, json) = cut(&state, -1).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["source"], -1);
    assert_eq!(state.program_bus.on_air_now().scene, None);
    let record = last_cut(&state);
    assert_eq!(record["scene"], "OBS manuál");
    assert_eq!(record["action"], "input");
    assert_eq!(record["source"], -1);
    assert_eq!(record["via"], "dashboard");
    assert_eq!(record["cg_forward"], Value::Null, "nothing went to cg OBS");
}

/// A playlist whose catalog names no scene (here: inactive) is cut with no
/// scene.
#[tokio::test]
async fn a_playlist_that_names_no_scene_is_cut_without_a_scene() {
    let state = test_state().await;
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
}

#[tokio::test]
async fn a_dashboard_cut_that_cannot_be_persisted_is_500_and_cuts_nothing() {
    let state = test_state().await;
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
