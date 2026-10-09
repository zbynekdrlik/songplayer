//! #223 S2: `GET /api/v1/program` and the cut answer carry `max`, the
//! `SP-program-MAX` telemetry, next to the existing blocks (additive: the
//! program fields stay flat). Through the real axum router.
//! Wired via `#[cfg(test)] #[path = "program_tests_max.rs"] mod tests_max;`.

use axum::http::StatusCode;
use sp_gpu::{ComposeStats, SpoutSendStats};

use super::tests::{add_playlist, call};
use crate::api::routes::tests::test_state;
use crate::playback::program_bus::PROGRAM_NDI_NAME;
use crate::playback::program_max::MAX_NOT_RUNNING;

#[tokio::test]
async fn get_program_reports_the_max_block() {
    let state = test_state().await;
    let (status, json) = call(state.clone(), "GET", "/api/v1/program", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json["max"],
        serde_json::json!({
            "enabled": false,
            "state": "off",
            "width": 3840,
            "height": 2160,
            "submitted": 0,
            "coalesced": 0,
            "failed": 0,
            "upload_us_p99": 0,
            "draw_us_p99": 0,
            "send_us_p99": 0,
            "send_at_us_p50": 0,
            "send_at_us_p99": 0,
            "send_at_us_max": 0,
            "send_late": 0,
            "vblank_output": null,
            "vblank_tracking": false,
            "vblank_period_ns": 0,
            "vblank_phase_us": 14000,
            "send_off_grid": 0,
            "send_phase_us_p50": 0,
            "send_phase_us_p99": 0,
            "slot_repicks": 0,
            "device_resets": 0,
            "sender_backoffs": 0,
            "spout_name": "SP-program-MAX",
            "adapter": null,
        }),
        "nothing started MAX in a unit test: the setting is not applied"
    );
    assert_eq!(json["ndi_name"], PROGRAM_NDI_NAME, "the program stays flat");
    assert!(json["outputs"].is_array(), "the other blocks stay");
    assert!(
        json.get("vban").is_none(),
        "#233: the VBAN telemetry is outputs[i].vban"
    );

    // An Arc of its own: the thread guard below must not borrow `state`,
    // which the cut call moves.
    let max = state.program_bus.max().clone();
    max.set_enabled(true);
    let (_, json) = call(state.clone(), "GET", "/api/v1/program", None).await;
    assert_eq!(json["max"]["enabled"], true);
    assert_eq!(json["max"]["state"], format!("error: {MAX_NOT_RUNNING}"));

    let _consumer = max.attach();
    max.record_sent(
        ComposeStats {
            upload_us: 700,
            draw_us: 300,
            uploads: 1,
        },
        SpoutSendStats { send_us: 900 },
    );
    max.record_device_reset();
    max.record_adapter("NVIDIA GeForce RTX 3070 Ti".into());
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
    let m = &json["max"];
    assert_eq!(m["state"], "running", "the cut answer carries it too");
    assert_eq!(m["submitted"], 1);
    assert_eq!(
        (&m["upload_us_p99"], &m["draw_us_p99"], &m["send_us_p99"]),
        (
            &serde_json::json!(700),
            &serde_json::json!(300),
            &serde_json::json!(900)
        )
    );
    assert_eq!(m["device_resets"], 1);
    assert_eq!(m["adapter"], "NVIDIA GeForce RTX 3070 Ti");
}

/// The setting saves through the settings API (`PATCH /api/v1/settings`,
/// a flat body) and loads back the way the settings task reads it.
#[tokio::test]
async fn the_max_setting_saves_through_the_settings_api_and_loads_back() {
    use crate::playback::program_max::load_max_enabled;
    let state = test_state().await;
    assert!(load_max_enabled(&state.pool).await.unwrap(), "default ON");
    let (status, _) = call(
        state.clone(),
        "PATCH",
        "/api/v1/settings",
        Some(serde_json::json!({ "program_max_enabled": "false" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        !load_max_enabled(&state.pool).await.unwrap(),
        "switched off"
    );
}
