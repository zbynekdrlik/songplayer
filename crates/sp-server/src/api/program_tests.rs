//! #209 `GET /api/v1/program` + `POST /api/v1/program/cut`, through the real
//! axum router. Shares `test_state`/`app` with `routes_tests.rs`.
//! Wired via `#[cfg(test)] #[path = "program_tests.rs"] mod tests;`.

use crate::api::routes::tests::{app, test_state};
use crate::playback::program_bus::{
    PROGRAM_NDI_NAME, ProgramBus, SETTING_PROGRAM_SOURCE, restore_selected_source,
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

pub(super) async fn add_playlist(pool: &sqlx::SqlitePool, name: &str) -> i64 {
    sqlx::query("INSERT INTO playlists (name, youtube_url, ndi_output_name) VALUES (?, ?, ?)")
        .bind(name)
        .bind(format!("https://youtube.com/playlist?list={name}"))
        .bind(format!("SP-{name}"))
        .execute(pool)
        .await
        .unwrap()
        .last_insert_rowid()
}

pub(super) async fn call(
    state: crate::AppState,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            req = req.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = app(state).oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
async fn get_program_reports_no_source_before_any_cut() {
    let state = test_state().await;
    let (status, json) = call(state, "GET", "/api/v1/program", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["ndi_name"], PROGRAM_NDI_NAME);
    assert!(json["source"].is_null());
    assert!(json["previous"].is_null());
    assert!(json["cut_boundary_100ns"].is_null());
    assert_eq!(json["health"]["forwarded"], 0);
    assert_eq!(json["health"]["filled"], 0);
    assert_eq!(json["health"]["cuts"], 0);
}

/// #221: the program answer has no `follow` block (L5 deleted the OBS
/// follow) and no `legacy_cg` record (B4 step 6), and its receiver
/// expectation is SP-program's own: while a source is on program, no NDI
/// receiver on `SP-program` is the `degraded_reason`; nothing on program, a
/// receiver connected, or no receiver poll yet (the count's 0 is no
/// reading, review round 1) is none.
#[tokio::test]
async fn the_program_answer_has_no_follow_no_legacy_cg_and_expects_sp_program_s_receiver() {
    let state = test_state().await;
    let (_, json) = call(state.clone(), "GET", "/api/v1/program", None).await;
    assert!(json.get("follow").is_none(), "{json}");
    assert!(json.get("legacy_cg").is_none(), "{json}");
    assert_eq!(
        json["degraded_reason"],
        serde_json::Value::Null,
        "nothing on program"
    );

    let slow = add_playlist(&state.pool, "slow").await;
    state.program_bus.select_initial(slow, Some("sp-slow"));
    let (_, json) = call(state.clone(), "GET", "/api/v1/program", None).await;
    assert_eq!(json["health"]["connections"], 0);
    assert_eq!(
        json["degraded_reason"],
        serde_json::Value::Null,
        "the sender has not polled its receivers yet"
    );

    state.program_bus.set_connections(0);
    let (_, json) = call(state.clone(), "GET", "/api/v1/program", None).await;
    assert_eq!(json["degraded_reason"], "no NDI receiver on SP-program");

    state.program_bus.set_connections(2);
    let (_, json) = call(state.clone(), "GET", "/api/v1/program", None).await;
    assert_eq!(json["degraded_reason"], serde_json::Value::Null);

    // "OBS manuál" on program expects a receiver on SP-program the same way.
    state.program_bus.set_connections(0);
    state.program_bus.select_initial(-1, None);
    let (_, json) = call(state, "GET", "/api/v1/program", None).await;
    assert_eq!(json["degraded_reason"], "no NDI receiver on SP-program");
}

#[tokio::test]
async fn cut_selects_the_source_get_reports_it_and_it_persists() {
    let state = test_state().await;
    let slow = add_playlist(&state.pool, "slow").await;
    let fast = add_playlist(&state.pool, "fast").await;

    let (status, json) = call(
        state.clone(),
        "POST",
        "/api/v1/program/cut",
        Some(serde_json::json!({ "source": slow })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["source"], slow);
    assert!(json["cut_boundary_100ns"].as_i64().is_some_and(|b| b > 0));
    assert_eq!(json["health"]["cuts"], 1);

    let (status, json) = call(
        state.clone(),
        "POST",
        "/api/v1/program/cut",
        Some(serde_json::json!({ "source": fast })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["source"], fast);
    assert_eq!(json["health"]["cuts"], 2);

    let (status, json) = call(state.clone(), "GET", "/api/v1/program", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["source"], fast);
    assert_eq!(json["health"]["cuts"], 2);
    assert_eq!(state.program_bus.status().source, Some(fast));

    // Persisted: a restart restores the selected source.
    let stored = crate::db::models::get_setting(&state.pool, SETTING_PROGRAM_SOURCE)
        .await
        .unwrap();
    assert_eq!(stored, Some(fast.to_string()));
    let restarted = ProgramBus::new();
    assert_eq!(
        restore_selected_source(&state.pool, &restarted).await,
        Some(fast)
    );
    assert_eq!(restarted.status().source, Some(fast));
}

#[tokio::test]
async fn a_cut_away_from_a_restored_source_reports_it_as_previous() {
    let state = test_state().await;
    let slow = add_playlist(&state.pool, "slow").await;
    let fast = add_playlist(&state.pool, "fast").await;
    state.program_bus.select_initial(slow, None); // as restored at startup
    let (status, json) = call(
        state.clone(),
        "POST",
        "/api/v1/program/cut",
        Some(serde_json::json!({ "source": fast })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["source"], fast);
    assert_eq!(
        json["previous"], slow,
        "slow owns every boundary before the cut"
    );
    let (_, json) = call(state, "GET", "/api/v1/program", None).await;
    assert_eq!(json["previous"], slow);
}

#[tokio::test]
async fn cut_to_an_unknown_playlist_is_404_and_changes_nothing() {
    let state = test_state().await;
    let (status, _) = call(
        state.clone(),
        "POST",
        "/api/v1/program/cut",
        Some(serde_json::json!({ "source": 4242 })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(state.program_bus.status().source, None);
    assert_eq!(
        crate::db::models::get_setting(&state.pool, SETTING_PROGRAM_SOURCE)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn cut_without_a_source_is_rejected() {
    let state = test_state().await;
    let (status, _) = call(
        state.clone(),
        "POST",
        "/api/v1/program/cut",
        Some(serde_json::json!({ "src": 1 })),
    )
    .await;
    assert!(status.is_client_error(), "got {status}");
    assert_eq!(state.program_bus.status().health.cuts, 0);
}

// --- #210: the VBAN block + its settings ------------------------------------

#[tokio::test]
async fn get_program_reports_the_vban_block() {
    use crate::playback::audio_out_block::ProgramBlock;
    use crate::playback::vban_out::tests::active_config;
    let state = test_state().await;
    let (status, json) = call(state.clone(), "GET", "/api/v1/program", None).await;
    assert_eq!(status, StatusCode::OK);
    let v = &json["vban"];
    assert_eq!(v["enabled"], false);
    assert_eq!(v["running"], false, "no VBAN thread in a unit test");
    assert_eq!(v["stream_name"], "sp-program");
    assert_eq!(v["packets_sent"], 0);
    assert_eq!(v["blocks_dropped"], 0);
    assert_eq!(v["late_sends"], 0);
    assert_eq!(v["send_interval_p99_us"], 0);
    assert_eq!(v["targets"], serde_json::json!([]));

    let vban = state.program_bus.vban();
    vban.set_config(active_config(&["127.0.0.1:6980"]));
    for due in 0..11 {
        vban.push(ProgramBlock::silence(due)); // one over the bound
    }
    let (status, json) = call(state.clone(), "GET", "/api/v1/program", None).await;
    assert_eq!(status, StatusCode::OK);
    let v = &json["vban"];
    assert_eq!(v["enabled"], true);
    assert_eq!(v["blocks_dropped"], 1);
    assert_eq!(
        v["targets"],
        serde_json::json!([{"target": "127.0.0.1:6980", "addr": "127.0.0.1:6980", "error": null}])
    );
    assert_eq!(
        json["ndi_name"], PROGRAM_NDI_NAME,
        "the program fields stay flat"
    );
    assert_eq!(json["health"]["cuts"], 0);

    let pid = add_playlist(&state.pool, "slow").await;
    let (status, json) = call(
        state,
        "POST",
        "/api/v1/program/cut",
        Some(serde_json::json!({ "source": pid })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["source"], pid);
    assert_eq!(
        json["vban"]["blocks_dropped"], 1,
        "the cut answer carries it too"
    );
}

#[tokio::test]
async fn the_vban_settings_save_through_the_settings_api_and_load_back() {
    use crate::playback::vban_out::load_vban_settings;
    let state = test_state().await;
    let (status, _) = call(
        state.clone(),
        "PATCH",
        "/api/v1/settings",
        Some(serde_json::json!({
            "vban_enabled": "true",
            "vban_stream_name": "sp-program",
            "vban_targets": "dev1.lan:6980, lv1.lan:6980",
        })),
    )
    .await;
    assert!(status.is_success(), "got {status}");
    let s = load_vban_settings(&state.pool).await.unwrap();
    assert!(s.enabled);
    assert_eq!(s.stream_name, "sp-program");
    assert_eq!(s.target_specs(), vec!["dev1.lan:6980", "lv1.lan:6980"]);
    let (_, json) = call(state.clone(), "GET", "/api/v1/settings", None).await;
    assert_eq!(json["vban_enabled"], "true");
    assert_eq!(json["vban_targets"], "dev1.lan:6980, lv1.lan:6980");

    let (status, _) = call(
        state.clone(),
        "PATCH",
        "/api/v1/settings",
        Some(serde_json::json!({ "vban_enabled": "false" })),
    )
    .await;
    assert!(status.is_success());
    assert!(!load_vban_settings(&state.pool).await.unwrap().enabled);
}

// --- #212: the NDI input "OBS manuál" -----------------------------------------

pub(super) async fn enable_input(state: &crate::AppState, source: &str) {
    let (status, _) = call(
        state.clone(),
        "PATCH",
        "/api/v1/settings",
        Some(serde_json::json!({
            "ndi_input_enabled": "true",
            "ndi_input_source": source,
        })),
    )
    .await;
    assert!(status.is_success(), "got {status}");
}

#[tokio::test]
async fn get_program_reports_the_input_block_from_the_stored_settings() {
    let state = test_state().await;
    let (status, json) = call(state.clone(), "GET", "/api/v1/program", None).await;
    assert_eq!(status, StatusCode::OK);
    let i = &json["input"];
    assert_eq!(i["id"], -1);
    assert_eq!(i["label"], "OBS manuál");
    assert_eq!(i["enabled"], false);
    assert_eq!(i["running"], false, "no input thread in a unit test");
    assert_eq!(i["connected"], false);
    assert_eq!(i["source"], "");
    assert_eq!(i["stream"], "");
    assert_eq!(i["frames_received"], 0);
    assert_eq!(i["video_repeats"], 0);
    assert_eq!(i["video_drops"], 0);
    assert_eq!(i["no_source_boundaries"], 0);
    assert_eq!(i["audio_queue_depth"], 0);
    assert_eq!(i["connects_pending"], 0);
    assert!(i["last_connect_ms"].is_null());
    assert!(i["last_close_ms"].is_null());
    assert!(i["last_frame_size"].is_null());
    assert_eq!(i["visible_sources"], serde_json::json!([]));

    enable_input(&state, " CG-OBS (manual) ").await;
    let (_, json) = call(state.clone(), "GET", "/api/v1/program", None).await;
    let i = &json["input"];
    assert_eq!(i["enabled"], true, "a save shows at once");
    assert_eq!(i["source"], "CG-OBS (manual)", "trimmed");
    assert_eq!(i["stream"], "manual");
    assert_eq!(
        json["ndi_name"], PROGRAM_NDI_NAME,
        "the program fields stay flat"
    );
}

#[tokio::test]
async fn the_input_settings_save_through_the_settings_api_and_load_back() {
    use crate::playback::ndi_input::{InputSettings, load_input_settings};
    let state = test_state().await;
    assert_eq!(
        load_input_settings(&state.pool).await.unwrap(),
        InputSettings::default(),
        "default: off, no source"
    );
    enable_input(&state, "CG-OBS (manual)").await;
    let s = load_input_settings(&state.pool).await.unwrap();
    assert!(s.enabled);
    assert_eq!(s.source, "CG-OBS (manual)");
    let (_, json) = call(state.clone(), "GET", "/api/v1/settings", None).await;
    assert_eq!(json["ndi_input_enabled"], "true");
    assert_eq!(json["ndi_input_source"], "CG-OBS (manual)");
    let (status, _) = call(
        state.clone(),
        "PATCH",
        "/api/v1/settings",
        Some(serde_json::json!({ "ndi_input_enabled": "false" })),
    )
    .await;
    assert!(status.is_success());
    let s = load_input_settings(&state.pool).await.unwrap();
    assert!(!s.enabled);
    assert_eq!(s.source, "CG-OBS (manual)", "the source is kept");
}

#[tokio::test]
async fn cut_to_the_input_is_404_while_it_is_disabled_and_changes_nothing() {
    let state = test_state().await;
    let (status, _) = call(
        state.clone(),
        "POST",
        "/api/v1/program/cut",
        Some(serde_json::json!({ "source": -1 })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(state.program_bus.status().source, None);
    assert_eq!(
        crate::db::models::get_setting(&state.pool, SETTING_PROGRAM_SOURCE)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn cut_to_an_enabled_input_without_a_source_is_404() {
    let state = test_state().await;
    enable_input(&state, "  ").await; // enabled, but nothing to receive
    let (status, _) = call(
        state.clone(),
        "POST",
        "/api/v1/program/cut",
        Some(serde_json::json!({ "source": -1 })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the input would never play");
    assert_eq!(state.program_bus.status().source, None);
}

#[tokio::test]
async fn cut_to_the_enabled_input_selects_it_and_persists_it() {
    let state = test_state().await;
    let slow = add_playlist(&state.pool, "slow").await;
    state.program_bus.select_initial(slow, None);
    enable_input(&state, "CG-OBS (manual)").await;
    let (status, json) = call(
        state.clone(),
        "POST",
        "/api/v1/program/cut",
        Some(serde_json::json!({ "source": -1 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["source"], -1);
    assert_eq!(json["previous"], slow);
    assert_eq!(json["input"]["enabled"], true, "the cut answer carries it");
    assert_eq!(state.program_bus.status().source, Some(-1));
    assert_eq!(
        crate::db::models::get_setting(&state.pool, SETTING_PROGRAM_SOURCE)
            .await
            .unwrap(),
        Some("-1".to_string())
    );
    // And back to the playlist.
    let (status, json) = call(
        state.clone(),
        "POST",
        "/api/v1/program/cut",
        Some(serde_json::json!({ "source": slow })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["source"], slow);
    assert_eq!(state.program_bus.status().source, Some(slow));
}

#[tokio::test]
async fn a_persisted_input_selection_restores_only_while_the_input_is_enabled() {
    let state = test_state().await;
    crate::db::models::set_setting(&state.pool, SETTING_PROGRAM_SOURCE, "-1")
        .await
        .unwrap();
    let disabled = ProgramBus::new();
    assert_eq!(restore_selected_source(&state.pool, &disabled).await, None);
    assert_eq!(
        disabled.status().source,
        None,
        "a disabled input is no source"
    );

    enable_input(&state, "").await; // enabled, but no source
    let sourceless = ProgramBus::new();
    assert_eq!(
        restore_selected_source(&state.pool, &sourceless).await,
        None
    );

    enable_input(&state, "CG-OBS (manual)").await;
    let enabled = ProgramBus::new();
    assert_eq!(
        restore_selected_source(&state.pool, &enabled).await,
        Some(-1)
    );
    assert_eq!(enabled.status().source, Some(-1));
}

#[tokio::test]
async fn a_persisted_playlist_restores_whatever_the_input_setting() {
    let state = test_state().await;
    crate::db::models::set_setting(&state.pool, SETTING_PROGRAM_SOURCE, "5")
        .await
        .unwrap();
    let bus = ProgramBus::new();
    assert_eq!(restore_selected_source(&state.pool, &bus).await, Some(5));
    assert_eq!(bus.status().source, Some(5));
}

// ---- #213: the `remote` block (the Companion remote control) ---------------

#[tokio::test]
async fn the_remote_block_reports_the_stored_settings_and_the_live_state() {
    let state = test_state().await;
    let (status, json) = call(state.clone(), "GET", "/api/v1/program", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json["remote"],
        serde_json::json!({
            "enabled": false,
            "port": 4456,
            "auth": false,
            "listening": false,
            "error": null,
            "clients": 0,
            "refused_over_cap": 0,
            "requests": 0,
            "last_request": null,
            "last_remote_cut": null,
            "unsupported_requests": [],
            "last_transition_duration": null,
            "program_scene": null,
        })
    );

    // The obs-websocket spec's example password (the only password in tests).
    for (key, value) in [
        ("remote_ws_enabled", "true"),
        ("remote_ws_port", "4460"),
        ("remote_ws_password", "supersecretpassword"),
    ] {
        crate::db::models::set_setting(&state.pool, key, value)
            .await
            .unwrap();
    }
    state.program_bus.remote().record_request("GetSceneList");
    let (_, json) = call(state.clone(), "GET", "/api/v1/program", None).await;
    assert_eq!(json["remote"]["enabled"], true);
    assert_eq!(json["remote"]["port"], 4460);
    assert_eq!(json["remote"]["auth"], true);
    assert_eq!(json["remote"]["requests"], 1);
    assert_eq!(
        json["remote"]["last_request"]["request_type"],
        "GetSceneList"
    );
    // The password itself is never part of the answer.
    assert!(!json.to_string().contains("supersecretpassword"));
}

// ---- #215: the `transition` block -----------------------------------------

#[tokio::test]
async fn the_program_reports_the_transition() {
    use crate::playback::program_transition::{SpecSource, TransitionSpec};
    let state = test_state().await;
    let (status, json) = call(state.clone(), "GET", "/api/v1/program", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json["transition"],
        serde_json::json!({
            "kind": "cut",
            "duration_ms": 0,
            "n_slots": 0,
            "source": "fallback",
            "active": null,
            "transitions_done": 0,
            "mixed_boundaries": 0,
            "side_fills": 0,
            "cue_wait_boundaries": 0,
            "cue_timeouts": 0,
        }),
        "until the transition-settings task sets one, a cut is a hard cut"
    );

    // A cut with a 300 ms fade in force opens a 9-slot window from the
    // program's source to the new one.
    let slow = add_playlist(&state.pool, "slow").await;
    let fast = add_playlist(&state.pool, "fast").await;
    state.program_bus.select_initial(slow, None);
    assert!(
        state
            .program_bus
            .set_transition(TransitionSpec::fade(300, SpecSource::Setting))
    );
    let (status, json) = call(
        state.clone(),
        "POST",
        "/api/v1/program/cut",
        Some(serde_json::json!({ "source": fast })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let start = json["cut_boundary_100ns"].as_i64().expect("a cut boundary");
    assert_eq!(
        json["transition"],
        serde_json::json!({
            "kind": "fade",
            "duration_ms": 300,
            "n_slots": 9,
            "source": "setting",
            "active": {
                "from": slow,
                "to": fast,
                "start_boundary_100ns": start,
                "n_slots": 9,
                "served_slots": 0,
                "progress": 0,
            },
            "transitions_done": 0,
            "mixed_boundaries": 0,
            "side_fills": 0,
            "cue_wait_boundaries": 0,
            "cue_timeouts": 0,
        })
    );
    let (_, json) = call(state, "GET", "/api/v1/program", None).await;
    assert_eq!(json["transition"]["active"]["to"], fast);
}

// ---- #221: the on-air publication -------------------------------------------

#[tokio::test]
async fn a_dashboard_cut_is_published_with_the_playlists_catalog_scene() {
    use crate::playback::program_on_air::program_scene_name;
    let state = test_state().await;
    let fast = add_playlist(&state.pool, "fast").await; // ndi_output_name SP-fast
    let (status, _) = call(
        state.clone(),
        "POST",
        "/api/v1/program/cut",
        Some(serde_json::json!({ "source": fast })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let on_air = state.program_bus.on_air_now();
    assert_eq!(
        (on_air.seq, on_air.source, on_air.scene.as_deref()),
        (1, Some(fast), Some("sp-fast"))
    );
    // The NDI input has no catalog scene: the resolver names it.
    enable_input(&state, "CG-OBS (manual)").await;
    let (status, _) = call(
        state.clone(),
        "POST",
        "/api/v1/program/cut",
        Some(serde_json::json!({ "source": -1 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let on_air = state.program_bus.on_air_now();
    assert_eq!(
        (on_air.seq, on_air.source, on_air.scene.as_deref()),
        (2, Some(-1), None)
    );
    assert_eq!(program_scene_name(&on_air).as_deref(), Some("OBS manuál"));
}

/// #221 L3: `remote.program_scene` is SP-program's scene from the one
/// resolver — what the facade answers `GetCurrentProgramScene` with and
/// feeds back to Companion — after a dashboard cut too.
#[tokio::test]
async fn the_remote_block_names_sp_programs_scene_after_a_dashboard_cut() {
    let state = test_state().await;
    let fast = add_playlist(&state.pool, "fast").await; // ndi_output_name SP-fast
    let (_, json) = call(state.clone(), "GET", "/api/v1/program", None).await;
    assert_eq!(json["remote"]["program_scene"], serde_json::Value::Null);
    let (status, json) = call(
        state.clone(),
        "POST",
        "/api/v1/program/cut",
        Some(serde_json::json!({ "source": fast })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["remote"]["program_scene"], "sp-fast");
    enable_input(&state, "CG-OBS (manual)").await;
    call(
        state.clone(),
        "POST",
        "/api/v1/program/cut",
        Some(serde_json::json!({ "source": -1 })),
    )
    .await;
    let (_, json) = call(state, "GET", "/api/v1/program", None).await;
    assert_eq!(json["remote"]["program_scene"], "OBS manuál");
}

/// #210: `health.timing` carries the `SP-program` sender's per-boundary
/// stage timing, under these names, so the box can read which stage made a
/// VBAN hand-off late.
#[tokio::test]
async fn get_program_reports_the_senders_boundary_timing() {
    let state = test_state().await;
    let b = 17_907_771_311_333_333;
    let late =
        state
            .program_bus
            .record_timing(&crate::playback::program_output_timing::BoundaryMarks {
                stamp_100ns: b,
                taken_100ns: b + 60_000,
                fed_100ns: b + 700_000,
                submit_start_100ns: b + 700_000,
                submitted_100ns: b + 770_000,
            });
    assert!(
        late.is_some(),
        "70 ms late: after its first packet was due (L = 66.7 ms), WARNed"
    );
    let (status, json) = call(state, "GET", "/api/v1/program", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json["health"]["timing"],
        serde_json::json!({
            "boundaries": 1,
            "ready_late_us_max": 6000,
            "vban_feed_late_us_max": 70000,
            "submit_us_max": 7000,
            "ready_late_over_5ms": 1,
            "vban_feed_late_over_5ms": 1,
            "submit_over_5ms": 1,
            "vban_feed_late_over_10ms": 1,
            "vban_feed_late_over_budget": 1,
            "warned": 1
        })
    );
}

/// #210 part 2: `vban.late_max_us` + `vban.late_events` carry the VBAN
/// thread's late packets under these names, each event `{utc_ms, late_us}`,
/// so the box names every stall by its instant without a dev1 capture.
#[tokio::test]
async fn get_program_reports_the_vban_threads_late_packets() {
    use crate::playback::audio_out_block::ProgramBlock;
    use crate::playback::vban_out::VbanSender;
    use crate::playback::vban_out::tests::{FakeClock, RecordingSink, active_config};
    use crate::playback::vban_packet::VBAN_SEND_LATENCY_100NS;
    let state = test_state().await;
    let (_, json) = call(state.clone(), "GET", "/api/v1/program", None).await;
    assert_eq!(json["vban"]["late_max_us"], 0);
    assert_eq!(json["vban"]["late_events"], serde_json::json!([]));

    let vban = state.program_bus.vban();
    vban.set_config(active_config(&["127.0.0.1:6980"]));
    let due: i64 = 17_907_771_311_333_333;
    // The thread reaches the block's packet 0 12 ms after it was due.
    let sent = due + VBAN_SEND_LATENCY_100NS + 120_000;
    let mut clock = FakeClock::at(sent);
    let mut sink = RecordingSink::on(&clock);
    VbanSender::default().send_block(vban, &ProgramBlock::silence(due), &mut sink, &mut clock);
    let (status, json) = call(state, "GET", "/api/v1/program", None).await;
    assert_eq!(status, StatusCode::OK);
    let utc_ms = sent.div_euclid(10_000);
    assert_eq!(json["vban"]["late_max_us"], 12_000);
    assert_eq!(
        json["vban"]["late_events"],
        serde_json::json!([
            {"utc_ms": utc_ms, "late_us": 12_000},
            {"utc_ms": utc_ms, "late_us": 7_833}
        ])
    );
}
