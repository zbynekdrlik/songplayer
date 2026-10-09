//! #221 L4a: `POST /api/v1/program/cut` goes through the ONE switch path
//! (`program_switch::switch_source`, `via=dashboard`), through the real axum
//! router: a playlist is cut with its catalog scene, -1 with none, the switch
//! waits for the `switch_order`, and (B4 step 6) cg OBS is told nothing — the
//! dashboard has no way to cg OBS at all any more. ROZHODNUTÉ 6022247729: a
//! cut to an inactive playlist or one whose catalog names no scene is refused
//! (409, nothing changes, a keep with its reason), and both program answers
//! list those playlists (`cut_refused`). Shares the helpers of
//! `program_tests.rs`.
//! Wired via `#[cfg(test)] #[path = "program_tests_switch.rs"] mod tests_switch;`.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::tests::{add_playlist, call, enable_input};
use crate::AppState;
use crate::api::routes::tests::{app, test_state};
use crate::db::models::get_setting;
use crate::playback::program_bus::SETTING_PROGRAM_SOURCE;

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

/// #245: `{"source": -2}` cuts to Blank, SongPlayer's own black — with no
/// playlist row and no NDI input needed — persisted, published and
/// recorded as `Blank`, and nothing goes to cg OBS.
#[tokio::test]
async fn a_dashboard_cut_to_blank_needs_no_playlist_and_no_input() {
    let state = test_state().await;
    let fast = add_playlist(&state.pool, "fast").await;
    assert_eq!(cut(&state, fast).await.0, StatusCode::OK);
    let (status, json) = cut(&state, -2).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    // (Both cuts land in the same slot in this rig, so the second replaces
    // the first: `previous` says nothing here.)
    assert_eq!(json["source"], json!(-2));
    assert_eq!(
        state.program_bus.on_air_now().scene.as_deref(),
        Some("Blank")
    );
    let persisted = get_setting(&state.pool, SETTING_PROGRAM_SOURCE)
        .await
        .unwrap();
    assert_eq!(persisted.as_deref(), Some("-2"));
    let record = last_cut(&state);
    assert_eq!(
        (
            &record["scene"],
            &record["action"],
            &record["source"],
            &record["via"]
        ),
        (
            &json!("Blank"),
            &json!("blank"),
            &json!(-2),
            &json!("dashboard")
        )
    );
    assert_eq!(record["cg_forward"], Value::Null, "nothing went to cg OBS");
    // Another negative id is no source.
    assert_eq!(cut(&state, -3).await.0, StatusCode::NOT_FOUND);
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

/// An active playlist with this NDI output name (`add_playlist` always names
/// it `SP-<name>`).
async fn add_named(state: &AppState, name: &str, ndi_output_name: &str) -> i64 {
    sqlx::query("INSERT INTO playlists (name, youtube_url, ndi_output_name) VALUES (?, ?, ?)")
        .bind(name)
        .bind(format!("https://youtube.com/playlist?list={name}"))
        .bind(ndi_output_name)
        .execute(&state.pool)
        .await
        .unwrap()
        .last_insert_rowid()
}

async fn set_active(state: &AppState, pid: i64, active: bool) {
    sqlx::query("UPDATE playlists SET is_active = ? WHERE id = ?")
        .bind(active)
        .bind(pid)
        .execute(&state.pool)
        .await
        .unwrap();
}

/// The cut's status and its body as text, whatever it is.
async fn cut_text(state: &AppState, source: i64) -> (StatusCode, String) {
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/program/cut")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "source": source }).to_string()))
        .unwrap();
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

/// #221 ROZHODNUTÉ 6022247729: a dashboard cut to `source` is refused with
/// 409 and changes NOTHING while `on_program` (published as `sp-fast`) is on
/// SP-program: the source, its on-air scene, the persisted source and the
/// cut counter stay. It is recorded as a keep with `reason`. Returns the
/// answer's `error` (the reason in words).
async fn assert_refused(state: &AppState, source: i64, reason: &str, on_program: i64) -> String {
    let cuts = state.program_bus.status().health.cuts;
    let (status, text) = cut_text(state, source).await;
    assert_eq!(status, StatusCode::CONFLICT, "playlist {source}: {text}");
    // Review round 2: the body names the reason code, so the dashboard can
    // say why in Slovak (`sp_core::program_refusal::refusal_text`).
    let body: Value = serde_json::from_str(&text).expect("a JSON refusal body");
    assert_eq!(body["reason"], reason, "{text}");
    let program = state.program_bus.status();
    assert_eq!(program.source, Some(on_program), "SP-program unchanged");
    assert_eq!(program.health.cuts, cuts, "nothing was cut");
    assert_eq!(
        state.program_bus.on_air_now().scene.as_deref(),
        Some("sp-fast")
    );
    let stored = get_setting(&state.pool, SETTING_PROGRAM_SOURCE)
        .await
        .unwrap();
    assert_eq!(stored, Some(on_program.to_string()), "nothing persisted");
    let record = last_cut(state);
    assert_eq!(record["action"], "keep");
    assert_eq!(record["reason"], reason);
    assert_eq!(record["scene"], source.to_string());
    assert_eq!(record["source"], Value::Null);
    assert_eq!(record["cut_boundary_100ns"], Value::Null);
    assert_eq!(record["via"], "dashboard");
    assert_eq!(record["cg_forward"], Value::Null);
    body["error"]
        .as_str()
        .expect("the reason in words")
        .to_string()
}

/// #221 ROZHODNUTÉ 6022247729 (replaces the L4a pin "a playlist whose catalog
/// names no scene: cut with no scene"): every consumer takes SP-program, so a
/// cut to an INACTIVE playlist (it has no output) would black them all at
/// once. It is refused.
#[tokio::test]
async fn a_dashboard_cut_to_an_inactive_playlist_is_refused_and_changes_nothing() {
    let state = test_state().await;
    let fast = add_playlist(&state.pool, "fast").await;
    let slow = add_playlist(&state.pool, "slow").await;
    set_active(&state, slow, false).await;
    let (status, _) = cut(&state, fast).await;
    assert_eq!(status, StatusCode::OK);
    let error = assert_refused(&state, slow, "playlist_inactive", fast).await;
    assert!(error.contains("inactive"), "a clear reason: {error}");
}

/// The same for an ACTIVE playlist whose catalog names no scene: one with no
/// NDI output name, and two that share one (ignoring ASCII case).
#[tokio::test]
async fn a_dashboard_cut_to_a_playlist_that_names_no_scene_is_refused_and_changes_nothing() {
    let state = test_state().await;
    let fast = add_playlist(&state.pool, "fast").await;
    let unnamed = add_named(&state, "unnamed", "").await;
    let dup_a = add_named(&state, "dup a", "SP-dup").await;
    let dup_b = add_named(&state, "dup b", "sp-DUP").await;
    let (status, _) = cut(&state, fast).await;
    assert_eq!(status, StatusCode::OK);
    for pid in [unnamed, dup_a, dup_b] {
        let error = assert_refused(&state, pid, "no_scene", fast).await;
        assert!(error.contains("names no scene"), "a clear reason: {error}");
    }
}

/// After a refusal the rest cuts as before: an active playlist with a scene,
/// and -1 ("OBS manuál") while the NDI input is a source.
#[tokio::test]
async fn after_a_refusal_a_playlist_with_a_scene_and_the_input_still_cut() {
    let state = test_state().await;
    let fast = add_playlist(&state.pool, "fast").await;
    let slow = add_playlist(&state.pool, "slow").await;
    let worship = add_playlist(&state.pool, "worship").await;
    set_active(&state, slow, false).await;
    let (status, _) = cut(&state, fast).await;
    assert_eq!(status, StatusCode::OK);
    assert_refused(&state, slow, "playlist_inactive", fast).await;
    let (status, json) = cut(&state, worship).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["source"], worship);
    assert_eq!(
        state.program_bus.on_air_now().scene.as_deref(),
        Some("sp-worship")
    );
    assert_eq!(last_cut(&state)["action"], "playlist");
    enable_input(&state, "CG-OBS (manual)").await;
    let (status, json) = cut(&state, -1).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["source"], -1);
    assert_eq!(last_cut(&state)["action"], "input");
}

/// Both program answers list the playlists a cut refuses, with the reason,
/// in id order, by the rule the cut itself applies: the dashboard disables
/// those buttons. A playlist with a scene is not listed, and a playlist made
/// active again leaves the list.
#[tokio::test]
async fn the_program_answers_list_the_playlists_a_cut_refuses() {
    let state = test_state().await;
    let fast = add_playlist(&state.pool, "fast").await;
    let slow = add_playlist(&state.pool, "slow").await;
    let unnamed = add_named(&state, "unnamed", "").await;
    set_active(&state, slow, false).await;
    let expected = json!([
        { "source": slow, "reason": "playlist_inactive" },
        { "source": unnamed, "reason": "no_scene" },
    ]);
    let (status, json) = call(state.clone(), "GET", "/api/v1/program", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["cut_refused"], expected);
    let (status, json) = cut(&state, fast).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["cut_refused"], expected, "the cut's answer too");
    set_active(&state, slow, true).await;
    let (_, json) = call(state.clone(), "GET", "/api/v1/program", None).await;
    let only_unnamed = json!([{ "source": unnamed, "reason": "no_scene" }]);
    assert_eq!(json["cut_refused"], only_unnamed);
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
