//! #233: the fan-out — every running output gets the same shared block, a
//! disabled entry nothing, a full queue never costs another output a block;
//! the state and latency each output reports. `entry` and `running_vban` are
//! `pub(crate)`: `program_output_tests_order.rs` and
//! `api/program_tests_outputs.rs` build their outputs with them.

use super::*;
use crate::playback::vban_out::tests::active_config;
use crate::playback::vban_out::{VbanSender, VbanTake};
use crate::playback::vban_packet::VbanFormat;
use crate::playback::vban_rate::VbanRateConverter;
use sp_core::audio_outputs::{RateChoice, VbanDest, VbanSampleFormat};
use std::time::Duration;

/// A VBAN entry `id` to 127.0.0.1:6980, at the network rate.
pub(crate) fn entry(id: &str) -> OutputEntry {
    OutputEntry::vban(
        id,
        id,
        VbanDest {
            host: "127.0.0.1".into(),
            port: 6980,
            stream_name: "sp-program".into(),
            format: VbanSampleFormat::Int24,
        },
    )
}

/// `out` running as the VBAN entry `id`, built for 48 kHz.
pub(crate) fn running_vban(id: &str, out: Arc<VbanOut>) -> RunningOutput {
    RunningOutput {
        entry: entry(id),
        built_rate: 48_000,
        sink: Some(OutputSink::Vban(out)),
        error: None,
    }
}

fn take(out: &VbanOut) -> ProgramBlock {
    match out.take_timeout(Duration::ZERO) {
        VbanTake::Block(b) => b,
        other => panic!("no block: {other:?}"),
    }
}

#[test]
fn every_running_output_gets_the_same_block() {
    let (a, b) = (Arc::new(VbanOut::new()), Arc::new(VbanOut::new()));
    let outputs = AudioOutputs::new();
    let mut off = entry("out-3");
    off.enabled = false;
    outputs.replace(vec![
        running_vban("out-1", a.clone()),
        running_vban("out-2", b.clone()),
        RunningOutput {
            entry: off,
            built_rate: 48_000,
            sink: None,
            error: None,
        },
    ]);
    let block = ProgramBlock {
        due_100ns: 7,
        samples: Some(vec![0.5; 3200].into()),
        substituted: false,
    };
    outputs.offer(&block);
    let (ga, gb) = (take(&a), take(&b));
    assert_eq!(ga, block);
    assert!(
        Arc::ptr_eq(ga.samples.as_ref().unwrap(), gb.samples.as_ref().unwrap()),
        "one copy, shared"
    );
    assert_eq!((a.queued(), b.queued()), (0, 0), "one block each");
}

#[test]
fn a_full_queue_on_one_output_never_costs_another_a_block() {
    let (stuck, live) = (Arc::new(VbanOut::new()), Arc::new(VbanOut::new()));
    let outputs = AudioOutputs::new();
    outputs.replace(vec![
        running_vban("out-1", stuck.clone()),
        running_vban("out-2", live.clone()),
    ]);
    for i in 0..15 {
        outputs.offer(&ProgramBlock::silence(i));
        assert_eq!(take(&live), ProgramBlock::silence(i));
    }
    assert_eq!(stuck.status().blocks_dropped, 5, "15 offered, bound 10");
    assert_eq!(live.status().blocks_dropped, 0);
}

#[test]
fn stop_all_stops_every_running_output_and_the_list_is_swapped_whole() {
    let (a, b) = (Arc::new(VbanOut::new()), Arc::new(VbanOut::new()));
    let outputs = AudioOutputs::default();
    assert_eq!(outputs.network_rate(), 48_000, "the default network rate");
    assert!(outputs.running().is_empty());
    assert!(outputs.problems().is_empty());
    outputs.replace(vec![running_vban("out-1", a.clone())]);
    outputs.replace(vec![running_vban("out-2", b.clone())]);
    outputs.offer(&ProgramBlock::silence(1));
    assert_eq!(a.queued(), 0, "the replaced list gets nothing");
    assert_eq!(b.queued(), 1);
    outputs.stop_all();
    assert_eq!(
        b.take_timeout(Duration::ZERO),
        VbanTake::Block(ProgramBlock::silence(1))
    );
    assert_eq!(b.take_timeout(Duration::ZERO), VbanTake::Stopped);
    assert_eq!(
        a.take_timeout(Duration::ZERO),
        VbanTake::Idle,
        "not in the list"
    );
}

#[test]
fn the_network_rate_and_the_problems_are_kept() {
    let outputs = AudioOutputs::new();
    outputs.set_network_rate(96_000);
    outputs.set_problems(vec!["entry 2 (id out-2): type must be vban".into()]);
    assert_eq!(outputs.network_rate(), 96_000);
    assert_eq!(
        outputs.problems(),
        vec!["entry 2 (id out-2): type must be vban".to_string()]
    );
}

#[test]
fn the_vban_state_table() {
    assert_eq!(
        vban_state(false, None, true, true, None),
        (STATE_DISABLED, None)
    );
    assert_eq!(
        vban_state(true, Some("VBAN carries no 32000 Hz"), false, false, None),
        (STATE_WAITING, Some("VBAN carries no 32000 Hz".to_string()))
    );
    assert_eq!(
        vban_state(true, None, false, true, None),
        (STATE_OPENING, None)
    );
    assert_eq!(
        vban_state(true, None, true, false, Some("no IPv4 address")),
        (STATE_WAITING, Some("no IPv4 address".to_string()))
    );
    assert_eq!(
        vban_state(true, None, true, false, None),
        (
            STATE_WAITING,
            Some("the address is not resolved yet".to_string())
        )
    );
    assert_eq!(
        vban_state(true, None, true, true, Some("old error")),
        (STATE_RUNNING, None)
    );
    assert_eq!(
        (STATE_RUNNING, STATE_OPENING, STATE_WAITING, STATE_DISABLED),
        ("running", "opening", "waiting", "disabled")
    );
}

#[test]
fn a_vban_outputs_latency_is_the_send_budget_the_delay_and_the_converter() {
    assert!((vban_latency_ms(0, 48_000) - 66.6666).abs() < 1e-3);
    assert!((vban_latency_ms(40, 48_000) - 106.6666).abs() < 1e-3);
    assert!((vban_latency_ms(0, 96_000) - (66.6666 + 16.6667)).abs() < 1e-3);
    assert!((vban_latency_ms(10, 44_100) - (66.6666 + 10.0 + 16.6667)).abs() < 1e-3);
}

#[test]
fn the_status_lists_every_entry_in_list_order() {
    let format = VbanFormat::new(96_000, VbanSampleFormat::Int24).unwrap();
    let out = Arc::new(VbanOut::for_destination(format, 0));
    out.set_config(active_config(&["127.0.0.1:6980"]));
    let mut fast = entry("out-1");
    fast.rate = RateChoice::Fixed(96_000);
    fast.delay_ms = 20;
    let mut off = entry("out-2");
    off.enabled = false;
    let mut broken = entry("out-3");
    broken.vban.as_mut().unwrap().format = VbanSampleFormat::Float32;
    let outputs = AudioOutputs::new();
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
        RunningOutput {
            entry: broken,
            built_rate: 32_000,
            sink: None,
            error: Some("VBAN carries no 32000 Hz".into()),
        },
    ]);
    let st = outputs.status();
    assert_eq!(st.len(), 3);
    assert_eq!(
        (st[0].id.as_str(), st[0].kind, st[0].rate, st[0].format),
        ("out-1", "vban", 96_000, "int24")
    );
    assert_eq!(st[0].name, "out-1");
    assert!(st[0].enabled);
    assert_eq!(st[0].state, STATE_OPENING, "no thread off Windows");
    assert_eq!((st[0].channels, st[0].delay_ms), (2, 20));
    assert!((st[0].latency_ms - (66.6666 + 20.0 + 16.6667)).abs() < 1e-3);
    assert_eq!(
        st[0].vban.as_ref().unwrap().targets[0].target,
        "127.0.0.1:6980"
    );
    assert_eq!(
        (st[1].state, st[1].blocks_sent, st[1].blocks_dropped),
        (STATE_DISABLED, 0, 0)
    );
    assert!(st[1].vban.is_none());
    assert_eq!(
        (st[2].state, st[2].reason.as_deref(), st[2].format),
        (STATE_WAITING, Some("VBAN carries no 32000 Hz"), "float32")
    );
}

#[test]
fn a_running_outputs_counters_and_address_reach_the_status() {
    let out = Arc::new(VbanOut::new());
    out.set_config(active_config(&["127.0.0.1:6980"]));
    let outputs = AudioOutputs::new();
    outputs.replace(vec![running_vban("out-1", out.clone())]);
    for i in 0..=out.bound() as i64 {
        outputs.offer(&ProgramBlock::silence(i)); // one over the bound
    }
    let st = &outputs.status()[0];
    assert_eq!((st.blocks_dropped, st.blocks_sent), (1, 0));
    let mut unresolved = active_config(&["127.0.0.1:6980"]);
    unresolved.targets[0].addr = None;
    unresolved.targets[0].error = Some("no IPv4 address".into());
    out.set_config(unresolved);
    let st = &outputs.status()[0];
    assert_eq!(st.state, STATE_OPENING, "the thread is not running");
    assert_eq!(st.vban.as_ref().unwrap().targets[0].addr, None);
}

#[test]
fn a_running_vban_output_waits_for_its_address_with_the_resolve_error() {
    use crate::playback::vban_out::run_vban_loop;
    use crate::playback::vban_out::tests::{FakeClock, RecordingSink};
    let out = Arc::new(VbanOut::new());
    let mut cfg = active_config(&["127.0.0.1:6980"]);
    cfg.targets[0].addr = None;
    cfg.targets[0].error = Some("no IPv4 address".into());
    out.set_config(cfg);
    let outputs = AudioOutputs::new();
    outputs.replace(vec![running_vban("out-1", out.clone())]);
    let looped = out.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut clock = FakeClock::at(0);
        let mut sink = RecordingSink::on(&clock);
        run_vban_loop(&looped, &mut sink, &mut clock);
        let _ = tx.send(());
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !out.is_running() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let st = outputs.status();
    assert_eq!(
        (st[0].state, st[0].reason.as_deref()),
        (STATE_WAITING, Some("no IPv4 address"))
    );
    out.set_config(active_config(&["127.0.0.1:6980"]));
    assert_eq!(outputs.status()[0].state, STATE_RUNNING);
    out.stop();
    rx.recv_timeout(Duration::from_secs(20))
        .expect("the loop stops");
}

#[test]
fn a_vban_output_whose_thread_could_not_start_says_why() {
    // #233 review round 6: a failed UDP bind or thread spawn (Windows) left
    // the output "opening" with no reason, for good.
    let out = Arc::new(VbanOut::new());
    out.set_config(active_config(&["127.0.0.1:6980"]));
    let why = "the VBAN thread did not start: binding the UDP socket failed: denied";
    out.set_start_error(why.to_string());
    assert_eq!(out.start_error().as_deref(), Some(why));
    let st = running_vban("out-1", out).status(48_000);
    assert_eq!((st.state, st.reason.as_deref()), (STATE_WAITING, Some(why)));
}

// #233 lane 3: ASIO outputs in the fan-out and the status.

fn dvs_entry() -> OutputEntry {
    OutputEntry::asio(
        "out-3",
        "DVS",
        sp_core::audio_outputs::AsioDest {
            driver: "Dante Virtual Soundcard (x64)".into(),
            channels: [0, 1],
        },
    )
}

fn running_asio(out: Arc<AsioOut>) -> RunningOutput {
    RunningOutput {
        entry: dvs_entry(),
        built_rate: 0,
        sink: Some(OutputSink::Asio(out)),
        error: None,
    }
}

/// An ASIO output whose driver opened with `opened` (one worker step).
fn opened_asio(opened: crate::playback::asio_out::Opened) -> Arc<AsioOut> {
    use crate::playback::asio_out::AsioWorker;
    use crate::playback::asio_out::fake::FakeDevice;
    let out = Arc::new(AsioOut::for_entry(&dvs_entry()).unwrap());
    let mut d = FakeDevice::answering(vec![Ok(opened)]);
    AsioWorker::new(0).step(&out, &mut d, 0, None);
    out
}

#[test]
fn the_status_notes_a_driver_rate_off_the_network_rate() {
    use crate::playback::asio_out::fake::dvs;
    let running = running_asio(opened_asio(dvs(48_000.0)));
    let at96 = running.status(96_000);
    assert_eq!(
        (at96.kind, at96.state, at96.rate, at96.format),
        ("asio", STATE_RUNNING, 48_000, "Int32LSB")
    );
    assert_eq!(
        (
            at96.id.as_str(),
            at96.name.as_str(),
            at96.channels,
            at96.delay_ms
        ),
        ("out-3", "DVS", 2, 0)
    );
    assert_eq!(
        at96.note.as_deref(),
        Some("the driver runs at 48000 Hz, the network at 96000 Hz")
    );
    let asio = at96.asio.as_ref().unwrap();
    assert_eq!(
        (asio.driver.as_str(), asio.channels, asio.driver_rate),
        ("Dante Virtual Soundcard (x64)", [0, 1], 48_000)
    );
    assert!(at96.vban.is_none());
    assert_eq!(running.status(48_000).note, None);
    // AudioOutputs::status reads the rate the list was applied with.
    let outputs = AudioOutputs::new();
    outputs.replace(vec![running]);
    outputs.set_network_rate(96_000);
    assert_eq!(outputs.status()[0].note, at96.note);
}

#[test]
fn the_status_notes_a_driver_buffer_over_a_third_of_a_slot() {
    use crate::playback::asio_out::fake::dvs;
    let mut big = dvs(48_000.0);
    big.buffer_frames = 2_048;
    let st = running_asio(opened_asio(big)).status(96_000);
    assert_eq!(
        st.note.as_deref(),
        Some(
            "the driver runs at 48000 Hz, the network at 96000 Hz; the driver's buffer of \
             2048 frames (42.7 ms) is over a third of a grid slot (11.1 ms): the drift servo \
             may re-centre often"
        )
    );
    let st = running_asio(opened_asio(big)).status(48_000);
    assert!(
        st.note
            .unwrap()
            .starts_with("the driver's buffer of 2048 frames")
    );
}

#[test]
fn a_waiting_asio_output_names_its_reason_and_notes_nothing() {
    use crate::playback::asio_out::AsioWorker;
    use crate::playback::asio_out::fake::{FakeDevice, dvs};
    use crate::playback::asio_state::{DeviceEvents, Reason};
    let out = Arc::new(AsioOut::for_entry(&dvs_entry()).unwrap());
    let mut d = FakeDevice::answering(vec![Ok(dvs(48_000.0))]);
    let mut w = AsioWorker::new(0);
    w.step(&out, &mut d, 0, None);
    d.events.push_back(DeviceEvents {
        reset: true,
        ..Default::default()
    });
    w.step(&out, &mut d, 1, None);
    let st = running_asio(out).status(96_000);
    assert_eq!(
        (st.state, st.reason.as_deref()),
        (STATE_WAITING, Some("the driver asked for a reset"))
    );
    assert_eq!(st.note, None, "a note only while it runs");
    assert_eq!(st.asio.unwrap().reason_code, Some(Reason::Reset.code()));
}

#[test]
fn an_asio_output_not_started_disabled_or_unbuilt_says_so() {
    let fresh = running_asio(Arc::new(AsioOut::for_entry(&dvs_entry()).unwrap()));
    let st = fresh.status(96_000);
    assert_eq!(
        (st.state, st.reason, st.rate, st.format),
        (STATE_OPENING, None, 0, "")
    );
    assert!(!fresh.start_failed());
    let out = Arc::new(AsioOut::for_entry(&dvs_entry()).unwrap());
    let why = "the ASIO worker did not start: spawning it failed: no memory";
    out.set_start_error(why.to_string());
    let failed = running_asio(out);
    assert!(failed.start_failed(), "rebuilt on the next pass");
    assert_eq!(
        (
            failed.status(96_000).state,
            failed.status(96_000).reason.as_deref()
        ),
        (STATE_WAITING, Some(why))
    );
    let mut off = dvs_entry();
    off.enabled = false;
    let disabled = RunningOutput {
        entry: off,
        built_rate: 0,
        sink: None,
        error: None,
    };
    let st = disabled.status(96_000);
    assert_eq!((st.state, st.kind), (STATE_DISABLED, "asio"));
    assert!(st.asio.is_none());
    let unbuilt = RunningOutput {
        entry: dvs_entry(),
        built_rate: 0,
        sink: None,
        error: Some("not an ASIO entry".into()),
    };
    assert_eq!(
        unbuilt.status(96_000).reason.as_deref(),
        Some("not an ASIO entry")
    );
}

#[test]
fn an_asio_output_takes_the_shared_block_and_stops_or_discards() {
    let out = Arc::new(AsioOut::for_entry(&dvs_entry()).unwrap());
    let vban = Arc::new(VbanOut::new());
    let outputs = AudioOutputs::new();
    outputs.replace(vec![
        running_vban("out-1", vban.clone()),
        running_asio(out.clone()),
    ]);
    outputs.offer(&ProgramBlock::silence(1));
    assert_eq!((out.queued(), vban.queued()), (1, 1));
    outputs.stop_all();
    outputs.offer(&ProgramBlock::silence(2));
    assert_eq!(out.queued(), 1, "a stopped output takes nothing more");
    let sink = OutputSink::Asio(out.clone());
    sink.discard();
    assert_eq!(out.queued(), 0, "a discard drops what is queued");
}

/// #233 release review: the code of a waiting VBAN output's reason.
#[test]
fn the_vban_reason_code_table() {
    assert_eq!(vban_reason_code(STATE_RUNNING, None, false), None);
    assert_eq!(
        vban_reason_code(STATE_OPENING, Some("not_built"), true),
        None
    );
    assert_eq!(vban_reason_code(STATE_DISABLED, None, true), None);
    assert_eq!(
        vban_reason_code(STATE_WAITING, Some("converter"), true),
        Some("converter")
    );
    assert_eq!(
        vban_reason_code(STATE_WAITING, None, true),
        Some("unresolved")
    );
    assert_eq!(
        vban_reason_code(STATE_WAITING, None, false),
        Some("resolving")
    );
}

/// Review round 1: every reason code a waiting VBAN output can carry has the
/// dashboard's Slovak (sp-core's one vocabulary), never "neznámy dôvod" — a
/// rename on either side fails here.
#[test]
fn every_vban_reason_code_has_its_dashboard_text() {
    use sp_core::audio_outputs_save::vban_reason_sk;
    let codes: Vec<&str> = [
        vban_reason_code(STATE_WAITING, Some(VBAN_NOT_BUILT), false),
        vban_reason_code(STATE_WAITING, Some(VBAN_NOT_STARTED), false),
        vban_reason_code(STATE_WAITING, Some(VBAN_CONVERTER), false),
        vban_reason_code(STATE_WAITING, None, true),
        vban_reason_code(STATE_WAITING, None, false),
    ]
    .into_iter()
    .map(Option::unwrap)
    .collect();
    for code in codes {
        assert_ne!(vban_reason_sk(code), vban_reason_sk(""), "{code}");
    }
}

/// #233 release review: a rate converter rubato refuses (it sends silence)
/// makes the output read waiting with its reason, never running.
#[test]
fn a_refused_rate_converter_makes_the_output_wait_with_its_reason() {
    let out = Arc::new(VbanOut::for_destination(
        VbanFormat::new(96_000, VbanSampleFormat::Int24).unwrap(),
        0,
    ));
    out.set_config(active_config(&["10.0.0.1:6980"]));
    let _sender = VbanSender::with_converter(&out, VbanRateConverter::refused("no FFT plan"));
    let status = running_vban("out-1", out).status(96_000);
    assert_eq!(status.state, STATE_WAITING);
    assert_eq!(
        status.reason.as_deref(),
        Some(
            "the 96000 Hz rate converter could not be built (no FFT plan): the output sends silence"
        )
    );
    assert_eq!(status.reason_code, Some("converter"));
}

/// #233 release review: an output that could not be built, and one whose
/// thread did not start, each name their code.
#[test]
fn a_waiting_vban_output_names_its_reason_code() {
    let not_built = RunningOutput {
        entry: entry("out-1"),
        built_rate: 32_000,
        sink: None,
        error: Some("VBAN carries no 32000 Hz".into()),
    };
    assert_eq!(not_built.status(48_000).reason_code, Some("not_built"));
    let out = Arc::new(VbanOut::new());
    out.set_start_error("the VBAN thread did not start: binding the UDP socket failed".into());
    let not_started = running_vban("out-2", out).status(48_000);
    assert_eq!(not_started.state, STATE_WAITING);
    assert_eq!(not_started.reason_code, Some("not_started"));
    let running = running_vban("out-3", Arc::new(VbanOut::new())).status(48_000);
    assert_eq!(running.reason_code, None, "opening: no reason");
}
