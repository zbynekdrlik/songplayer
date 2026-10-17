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
    // #239: `fhd` is compared on its own (one json! literal of both would
    // nest the macro deep).
    let mut max_block = json["max"].clone();
    let fhd = max_block.as_object_mut().expect("an object").remove("fhd");
    let video_quiet = max_block
        .as_object_mut()
        .expect("an object")
        .remove("video_quiet");
    assert_eq!(
        max_block,
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
            "upload_mb_s": 0.0,
            "uploads_per_s": 0.0,
            "send_at_us_p50": 0,
            "send_at_us_p99": 0,
            "send_at_us_max": 0,
            "send_late": 0,
            "vblank_output": null,
            "vblank_state": null,
            "vblank_tracking": false,
            "vblank_period_ns": 0,
            "vblank_phase_us": 14000,
            "compose_phase_us": 13000,
            "send_off_grid": 0,
            "send_phase_us_p50": 0,
            "send_phase_us_p99": 0,
            "compose_phase_us_p50": 0,
            "compose_phase_us_p99": 0,
            "compose_late": 0,
            "slot_repicks": 0,
            "device_resets": 0,
            "sender_backoffs": 0,
            "spout_relists": 0,
            "spout_name": "SP-program-MAX",
            "adapter": null,
            "list_holder": null,
        }),
        "nothing started MAX in a unit test: the setting is not applied"
    );
    assert_eq!(
        fhd,
        Some(serde_json::json!({
            "enabled": false,
            "state": "off",
            "reason": "max_off",
            "spout_name": "SP-program",
            "listed_width": 0,
            "listed_height": 0,
            "submitted": 0,
            "failed": 0,
            "sender_backoffs": 0,
            "spout_relists": 0,
            "draw_us_p99": 0,
            "send_us_p99": 0,
        })),
        "#239: the SP-program Spout sender, off while MAX is; #251 step 2: no upload of its own"
    );
    assert_eq!(
        video_quiet,
        Some(serde_json::json!({
            "from_us": 3000,
            "to_us": 10500,
            "grid": false,
            "waits": 0,
            "wait_us_max": 0,
        })),
        "#251 step 4: the video side's quiet window, no grid while MAX is off"
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
            upload_bytes: 3_110_400,
            ..ComposeStats::default()
        },
        SpoutSendStats {
            send_us: 900,
            relisted: false,
        },
    );
    // #279 P0: two boundaries 100 ms apart, an FHD picture each.
    let t0 = std::time::Instant::now();
    max.record_uploads(t0, 3_110_400, 1);
    max.record_uploads(t0 + std::time::Duration::from_millis(100), 3_110_400, 1);
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
    assert_eq!(
        (&m["upload_mb_s"], &m["uploads_per_s"]),
        (&serde_json::json!(31.1), &serde_json::json!(10.0)),
        "#279 P0: 3 110 400 bytes, one picture, in 100 ms"
    );
}

/// #243 follow-up: `max.list_holder` names the program that kept Spout's
/// sender list one sender backoff (3 s): its program, pid and how long.
#[tokio::test]
async fn get_program_names_the_program_that_keeps_spout_s_list() {
    use std::time::{Duration, Instant};

    use crate::playback::program_max_list::tests::FakeList;
    use crate::playback::program_max_worker::MaxWorker;
    use crate::playback::program_max_worker::tests::{FakeGpu, picture, plain};

    let state = test_state().await;
    let max = state.program_bus.max().clone();
    max.set_enabled(true);
    let list = FakeList::default();
    let pid = std::process::id() ^ 1;
    list.held_by(Some(sp_gpu::LockHolder {
        pid,
        image: Some("Arena.exe".to_string()),
    }));
    let mut worker = MaxWorker::new(&max, FakeGpu::default()).with_list(Box::new(list));
    let t0 = Instant::now();
    let a = picture(4, 2);
    worker.serve(&plain(1, &a), t0);
    worker.serve(&plain(2, &a), t0 + Duration::from_millis(3_500));
    drop(worker);
    let (status, json) = call(state, "GET", "/api/v1/program", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json["max"]["list_holder"],
        serde_json::json!({"image": "Arena.exe", "pid": pid, "since_s": 3})
    );
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

/// #239: `max.fhd` is the `SP-program` Spout sender's block: off with reason
/// `max_off` while MAX is off, running with no reason once both switches
/// are on and the thread runs, its own counters; the cut answer carries it.
#[tokio::test]
async fn get_program_reports_the_fhd_spout_sender() {
    let state = test_state().await;
    let max = state.program_bus.max().clone();
    max.set_fhd_enabled(true);
    let (_, json) = call(state.clone(), "GET", "/api/v1/program", None).await;
    assert_eq!(json["max"]["fhd"]["enabled"], true);
    assert_eq!(json["max"]["fhd"]["state"], "off", "MAX is off");
    assert_eq!(json["max"]["fhd"]["reason"], "max_off");

    max.set_enabled(true);
    let _consumer = max.attach();
    max.record_fhd_sent(
        400,
        SpoutSendStats {
            send_us: 500,
            relisted: false,
        },
    );
    max.record_fhd_listed(Some((1920, 1080)));
    let pid = add_playlist(&state.pool, "slow").await;
    let (status, json) = call(
        state,
        "POST",
        "/api/v1/program/cut",
        Some(serde_json::json!({ "source": pid })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let fhd = &json["max"]["fhd"];
    assert_eq!(fhd["state"], "running", "the cut answer carries it too");
    assert_eq!(fhd["reason"], serde_json::Value::Null);
    assert_eq!(
        (&fhd["submitted"], &fhd["draw_us_p99"], &fhd["send_us_p99"]),
        (
            &serde_json::json!(1),
            &serde_json::json!(400),
            &serde_json::json!(500)
        )
    );
    assert!(
        fhd.get("upload_us_p99").is_none(),
        "#251 step 2: the FHD target uploads nothing of its own: {fhd}"
    );
    assert_eq!(
        (&fhd["listed_width"], &fhd["listed_height"]),
        (&serde_json::json!(1920), &serde_json::json!(1080))
    );
    assert_eq!(json["max"]["submitted"], 0, "MAX's own count is untouched");
}

/// #239: the FHD setting saves through the settings API and loads back the
/// way the settings task reads it.
#[tokio::test]
async fn the_fhd_setting_saves_through_the_settings_api_and_loads_back() {
    use crate::playback::program_max::load_fhd_enabled;
    let state = test_state().await;
    assert!(load_fhd_enabled(&state.pool).await.unwrap(), "default ON");
    let (status, _) = call(
        state.clone(),
        "PATCH",
        "/api/v1/settings",
        Some(serde_json::json!({ "program_spout_fhd_enabled": "false" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        !load_fhd_enabled(&state.pool).await.unwrap(),
        "switched off"
    );
}
