//! #233: `GET /api/v1/program` lists every audio output (`outputs[]`) with its
//! state, rate, format, latency and telemetry; the network rate; the stored
//! entries this version could not run. The top-level `vban` block is gone
//! (main-session ruling 5: its telemetry is `outputs[i].vban`).

use std::collections::HashMap;
use std::sync::Arc;

use axum::http::StatusCode;
use sp_core::audio_outputs::{OutputEntry, RateChoice, VbanDest, VbanSampleFormat};

use super::tests::{add_playlist, call};
use crate::api::routes::tests::test_state;
use crate::playback::audio_out::tests::running_vban;
use crate::playback::audio_out::{OutputSink, RunningOutput};
use crate::playback::audio_out_block::ProgramBlock;
use crate::playback::vban_out::VbanOut;
use crate::playback::vban_out::tests::active_config;
use crate::playback::vban_packet::VbanFormat;

async fn get_program(state: &crate::AppState) -> serde_json::Value {
    let (status, json) = call(state.clone(), "GET", "/api/v1/program", None).await;
    assert_eq!(status, StatusCode::OK);
    json
}

fn foh(id: &str) -> OutputEntry {
    OutputEntry::vban(
        id,
        "FOH",
        VbanDest {
            host: "127.0.0.1".into(),
            port: 6980,
            stream_name: "sp-program".into(),
            format: VbanSampleFormat::Int24,
        },
    )
}

#[tokio::test]
async fn get_program_lists_every_output_with_its_telemetry() {
    let state = test_state().await;
    let json = get_program(&state).await;
    assert_eq!(json["outputs"], serde_json::json!([]), "no output yet");
    assert_eq!(json["audio_network_rate"], 48_000);
    let out = Arc::new(VbanOut::for_destination(
        VbanFormat::new(96_000, VbanSampleFormat::Int24).unwrap(),
        400_000,
    ));
    out.set_config(active_config(&["127.0.0.1:6980"]));
    for i in 0..=out.bound() as i64 {
        out.push(ProgramBlock::silence(i)); // one over the bound
    }
    let mut fast = foh("out-1");
    fast.rate = RateChoice::Fixed(96_000);
    fast.delay_ms = 40;
    let mut off = foh("out-2");
    off.enabled = false;
    let outputs = state.program_bus.outputs();
    outputs.replace(vec![
        RunningOutput {
            entry: fast,
            built_rate: 96_000,
            sink: Some(OutputSink::Vban(out)),
            error: None,
        },
        RunningOutput {
            entry: off,
            built_rate: 48_000,
            sink: None,
            error: None,
        },
    ]);
    outputs.set_network_rate(96_000);

    let json = get_program(&state).await;
    assert!(
        json.get("vban").is_none(),
        "the #210 block moved under outputs[]"
    );
    assert_eq!(json["audio_network_rate"], 96_000);
    assert_eq!(json["outputs_problems"], serde_json::json!([]));
    let o = &json["outputs"][0];
    assert_eq!(o["id"], "out-1");
    assert_eq!(o["type"], "vban");
    assert_eq!(o["name"], "FOH");
    assert_eq!(o["enabled"], true);
    assert_eq!(o["state"], "opening");
    assert_eq!(o["reason"], serde_json::Value::Null);
    assert_eq!(o["rate"], 96_000);
    assert_eq!(o["format"], "int24");
    assert_eq!(o["channels"], 2);
    assert_eq!(o["delay_ms"], 40);
    let latency = o["latency_ms"].as_f64().unwrap();
    assert!(
        (latency - (66.6666 + 40.0 + 16.6667)).abs() < 1e-3,
        "{latency}"
    );
    assert_eq!(o["blocks_dropped"], 1);
    assert_eq!(o["blocks_sent"], 0);
    assert_eq!(o["vban"]["stream_name"], "sp-program");
    assert_eq!(o["vban"]["enabled"], true);
    assert_eq!(o["vban"]["running"], false, "no VBAN thread in a unit test");
    assert_eq!(o["vban"]["blocks_dropped"], 1);
    assert_eq!(
        o["vban"]["targets"],
        serde_json::json!([{"target": "127.0.0.1:6980", "addr": "127.0.0.1:6980", "error": null}])
    );
    for key in [
        "blocks_sent",
        "packets_sent",
        "send_errors",
        "blocks_substituted",
        "late_sends",
        "late_max_us",
        "late_events",
        "send_interval_p99_us",
        "frame_counter",
        "slew_owed_us",
    ] {
        assert!(o["vban"].get(key).is_some(), "vban.{key}");
    }
    let off = &json["outputs"][1];
    assert_eq!(
        (off["id"].as_str(), off["state"].as_str()),
        (Some("out-2"), Some("disabled"))
    );
    assert!(off.get("vban").is_none());
    assert_eq!(
        json["ndi_name"],
        crate::playback::program_bus::PROGRAM_NDI_NAME,
        "the program fields stay flat"
    );
}

#[tokio::test]
async fn the_cut_answer_carries_the_outputs_too() {
    let state = test_state().await;
    state
        .program_bus
        .outputs()
        .replace(vec![running_vban("out-1", Arc::new(VbanOut::new()))]);
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
    assert_eq!(json["outputs"][0]["id"], "out-1");
    assert_eq!(json["audio_network_rate"], 48_000);
    assert!(json.get("vban").is_none());
}

#[tokio::test]
async fn get_program_names_a_stored_entry_it_could_not_read() {
    let state = test_state().await;
    let raw = format!(
        "[{},{}]",
        serde_json::to_string(&foh("out-1")).unwrap(),
        r#"{"id":"out-2","name":"AES67","type":"aes67"}"#
    );
    crate::db::models::set_setting(&state.pool, "audio_outputs", &raw)
        .await
        .unwrap();
    let settings = crate::playback::audio_out_config::load(&state.pool)
        .await
        .unwrap();
    crate::playback::audio_out_task::apply(
        state.program_bus.outputs(),
        settings,
        &mut HashMap::new(),
        &|_: &OutputSink, _: &str| {},
    )
    .await;
    let json = get_program(&state).await;
    assert_eq!(
        json["outputs"].as_array().unwrap().len(),
        1,
        "the VBAN entry runs"
    );
    assert_eq!(json["outputs"][0]["id"], "out-1");
    assert_eq!(
        json["outputs_problems"],
        serde_json::json!(["entry 2 (id out-2): type must be vban or asio"])
    );
}

/// #210 part 2: `late_max_us` + `late_events` carry the VBAN thread's late
/// packets under these names, each event `{utc_ms, late_us}`, so the box
/// names every stall by its instant without a dev1 capture (#233: under the
/// output's `outputs[i].vban`).
#[tokio::test]
async fn get_program_reports_the_vban_threads_late_packets() {
    use crate::playback::vban_out::VbanSender;
    use crate::playback::vban_out::tests::{FakeClock, RecordingSink};
    use crate::playback::vban_packet::VBAN_SEND_LATENCY_100NS;
    let state = test_state().await;
    let vban = Arc::new(VbanOut::new());
    vban.set_config(active_config(&["127.0.0.1:6980"]));
    state
        .program_bus
        .outputs()
        .replace(vec![running_vban("out-1", vban.clone())]);
    let json = get_program(&state).await;
    assert_eq!(json["outputs"][0]["vban"]["late_max_us"], 0);
    assert_eq!(
        json["outputs"][0]["vban"]["late_events"],
        serde_json::json!([])
    );

    let due: i64 = 17_907_771_311_333_333;
    // The thread reaches the block's packet 0 12 ms after it was due.
    let sent = due + VBAN_SEND_LATENCY_100NS + 120_000;
    let mut clock = FakeClock::at(sent);
    let mut sink = RecordingSink::on(&clock);
    VbanSender::default().send_block(&vban, &ProgramBlock::silence(due), &mut sink, &mut clock);
    let json = get_program(&state).await;
    let utc_ms = sent.div_euclid(10_000);
    let v = &json["outputs"][0]["vban"];
    assert_eq!(v["late_max_us"], 12_000);
    assert_eq!(
        v["late_events"],
        serde_json::json!([
            {"utc_ms": utc_ms, "late_us": 12_000},
            {"utc_ms": utc_ms, "late_us": 7_833}
        ])
    );
    assert_eq!(json["outputs"][0]["blocks_sent"], 1);
}

/// #233 lane 3: off Windows an ASIO output never opens; it says why (the
/// production starter). On Windows a real worker opens the named driver: the
/// Windows job's `asio_win` tests and the box gate cover that path.
#[cfg(not(windows))]
#[tokio::test]
async fn an_asio_entry_off_windows_waits_and_says_why() {
    let state = test_state().await;
    let dvs = OutputEntry::asio(
        "out-3",
        "DVS",
        sp_core::audio_outputs::AsioDest {
            driver: "Dante Virtual Soundcard (x64)".into(),
            channels: [0, 1],
        },
    );
    let settings = crate::playback::audio_out_config::OutputsSettings {
        entries: vec![dvs],
        network_rate: 96_000,
        problems: vec![],
        not_a_list: false,
    };
    crate::playback::audio_out_task::apply(
        state.program_bus.outputs(),
        settings,
        &mut HashMap::new(),
        &crate::playback::audio_out_task::start_output_thread,
    )
    .await;
    let json = get_program(&state).await;
    let o = &json["outputs"][0];
    assert_eq!(
        (o["type"].as_str(), o["state"].as_str()),
        (Some("asio"), Some("waiting"))
    );
    assert_eq!(o["reason"], "ASIO runs on Windows only");
    assert_eq!(o["rate"], 0);
    assert!(o.get("note").is_none());
    let a = &o["asio"];
    assert_eq!(a["driver"], "Dante Virtual Soundcard (x64)");
    assert_eq!(a["channels"], serde_json::json!([0, 1]));
    assert_eq!(a["reason_code"], "windows_only");
    for key in [
        "driver_rate",
        "buffer_frames",
        "out_channels",
        "sample_type",
        "ppm",
        "rate_ppm",
        "locked",
        "latency_ms",
        "underruns",
        "resets",
        "offset_ms",
        "slew_eta_s",
        "cushion_ms",
        "hard_recentres",
        "last_hard_recentre",
        "overflows",
        "overloads",
        "retry_in_s",
        "clock_waits",
    ] {
        assert!(a.get(key).is_some(), "asio.{key}");
    }
}
