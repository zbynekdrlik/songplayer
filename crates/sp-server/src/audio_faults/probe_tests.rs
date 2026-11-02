//! #257: every audio path as the ledger reads it — which counter is which
//! kind and severity, the events, the states, the meters, the gate.

use std::collections::BTreeSet;
use std::sync::Arc;

use sp_core::config::{PROGRAM_BLANK_ID, PROGRAM_INPUT_ID, PROGRAM_INPUT_LABEL};

use super::*;
use sp_core::dante_status::DanteFigures;

use crate::audio_faults::model::{Counter, Event, Meter, Probe, Severity, StateFlag, kinds};
use crate::playback::asio_loop::{LoopEvent, LoopEventKind, LoopState, LoopStatus};
use crate::playback::asio_out::AsioStatus;
use crate::playback::asio_state::Reason;
use crate::playback::audio_out::OutputStatus;
use crate::playback::ndi_health::{
    AudioStats, NdiHealthRegistry, PacingStats, PipelineHealthSnapshot, PlaybackStateLabel,
};
use crate::playback::ndi_input::{InputSettings, NdiInputStatus};
use crate::playback::program_bus::ProgramBus;
use crate::playback::program_output_timing::BoundaryMarks;
use crate::playback::program_trace::{JobShape, ProgramTrace, TraceKind};
use crate::playback::program_transition::ActiveWindow;
use crate::playback::vban_out::VbanStatus;
use crate::playback::vban_stall::VbanLateEvent;

#[test]
fn seconds_read_as_whole_ms() {
    assert_eq!(ms(1.5), 1_500);
    assert_eq!(ms(0.0004), 0);
    assert_eq!(ms(f64::NAN), 0);
}

#[test]
fn the_host_down_code_is_the_reasons_own() {
    assert_eq!(HOST_DOWN_CODE, Reason::HostDown(String::new()).code());
}

/// A running fade from `from` to `to`.
fn fade(from: Option<i64>, to: i64) -> ActiveWindow {
    ActiveWindow {
        from,
        to,
        start_boundary_100ns: 0,
        n_slots: 9,
        served_slots: 4,
        progress: 44,
    }
}

/// Review round 1 (🔵 10): the outgoing side of a running fade is still
/// mixed into the program (up to ~10.5 s), so its faults count.
#[test]
fn on_program_is_the_selected_source_and_a_running_fades_outgoing_side() {
    let mut status = ProgramBus::new().status();
    assert_eq!(on_program(&status), BTreeSet::new(), "nothing selected");
    status.source = Some(5);
    assert_eq!(on_program(&status), BTreeSet::from([5]));
    status.transition.active = Some(fade(Some(3), 5));
    assert_eq!(on_program(&status), BTreeSet::from([3, 5]));
    status.transition.active = Some(fade(None, 5));
    assert_eq!(
        on_program(&status),
        BTreeSet::from([5]),
        "a fade up from nothing"
    );
    status.source = Some(PROGRAM_BLANK_ID);
    status.transition.active = Some(fade(Some(PROGRAM_INPUT_ID), PROGRAM_BLANK_ID));
    assert_eq!(
        on_program(&status),
        BTreeSet::from([PROGRAM_BLANK_ID, PROGRAM_INPUT_ID]),
        "the NDI input fading out to Blank"
    );
}

/// Review round 1: a coalesced boundary was never served (the sender's
/// full queue dropped the oldest unsent one; VBAN has no gap detection), a
/// loss (🟡 1); a resync counts only over a real source (🔵 11).
#[test]
fn the_program_loses_its_owned_fills_and_resyncs_and_coalesced_boundaries() {
    let mut status = ProgramBus::new().status();
    let h = &mut status.health;
    h.submitted = 900;
    h.filled = 50;
    h.filled_owned = 3;
    h.resyncs = 4;
    h.resyncs_owned = 2;
    h.late_dropped = 5;
    h.coalesced = 6;
    h.timing.vban_feed_late_over_budget = 7;
    assert_eq!(
        program_probe(&status, 8, 9),
        Probe {
            path: "program".to_string(),
            detail: "SP-program".to_string(),
            run: 900,
            gated_in: true,
            counters: vec![
                Counter::loss(kinds::FILLED, 3),
                Counter::loss(kinds::RESYNC, 2),
                Counter::risk(kinds::LATE_DROPPED, 5),
                Counter::loss(kinds::COALESCED, 6),
                Counter::risk(kinds::VBAN_FEED_LATE, 7),
                Counter::risk(kinds::LIMITED, 8),
                Counter::risk(kinds::NDI_LATE, 9),
            ],
            ..Probe::default()
        }
    );
}

/// An output's line as `GET /api/v1/program` serves it, with no transport
/// block.
fn output(id: &str, enabled: bool, sent: u64, dropped: u64) -> OutputStatus {
    OutputStatus {
        id: id.to_string(),
        kind: "vban",
        name: format!("Výstup {id}"),
        enabled,
        state: "running",
        reason: None,
        reason_code: None,
        rate: 96_000,
        format: "int24",
        channels: 2,
        delay_ms: 0,
        latency_ms: 83.4,
        blocks_sent: sent,
        blocks_dropped: dropped,
        vban: None,
        note: None,
        asio: None,
        dante: None,
    }
}

fn vban_status(late_events: Vec<VbanLateEvent>) -> VbanStatus {
    VbanStatus {
        enabled: true,
        running: true,
        stream_name: "sp-program".to_string(),
        blocks_sent: 300,
        packets_sent: 4_800,
        send_errors: 2,
        blocks_dropped: 1,
        blocks_substituted: 5,
        late_sends: 40,
        late_max_us: 12_001,
        late_events,
        send_interval_p99_us: 2_100,
        frame_counter: 4_800,
        slew_owed_us: 0,
        targets: Vec::new(),
    }
}

#[test]
fn a_vban_output_loses_its_drops_substitutions_and_send_errors_and_risks_a_late_packet() {
    let mut out = output("out-1", true, 300, 1);
    out.vban = Some(vban_status(vec![
        VbanLateEvent {
            utc_ms: 1_000,
            late_us: 10_000,
        },
        VbanLateEvent {
            utc_ms: 2_000,
            late_us: 10_001,
        },
    ]));
    assert_eq!(
        output_probes(&[out]),
        vec![Probe {
            path: "out:out-1".to_string(),
            detail: "Výstup out-1".to_string(),
            run: 300,
            gated_in: true,
            counters: vec![
                Counter::loss(kinds::BLOCKS_DROPPED, 1),
                Counter::loss(kinds::BLOCKS_SUBSTITUTED, 5),
                Counter::loss(kinds::SEND_ERROR, 2),
            ],
            events: vec![Event {
                kind: kinds::LATE_PACKET,
                severity: Severity::Risk,
                utc_ms: 2_000,
                amount: 10_001,
            }],
            ..Probe::default()
        }],
        "10 ms late exactly is no late packet"
    );
}

/// Review round 1: an output is gated in only while it runs (🟡 4: an
/// ASIO output's start-up and driver open drop blocks), and the
/// post-deploy probe's own `e2e-…` outputs are no path (🔵 13).
#[test]
fn a_disabled_or_probe_output_is_no_path_and_only_a_running_one_is_gated_in() {
    let waiting = OutputStatus {
        state: "waiting",
        ..output("waiting", true, 0, 4)
    };
    let opening = OutputStatus {
        state: "opening",
        ..output("opening", true, 0, 2)
    };
    let probes = output_probes(&[
        output("off", false, 0, 0),
        output("e2e-96k", true, 10, 1),
        waiting,
        opening,
        output("out-1", true, 30, 0),
    ]);
    let paths: Vec<(&str, bool)> = probes
        .iter()
        .map(|p| (p.path.as_str(), p.gated_in))
        .collect();
    assert_eq!(
        paths,
        vec![
            ("out:waiting", false),
            ("out:opening", false),
            ("out:out-1", true)
        ]
    );
    assert_eq!(
        probes[0],
        Probe {
            path: "out:waiting".to_string(),
            detail: "Výstup waiting".to_string(),
            run: 0,
            gated_in: false,
            counters: vec![Counter::loss(kinds::BLOCKS_DROPPED, 4)],
            ..Probe::default()
        },
        "its drops are read: the ledger takes them, never records them"
    );
}

fn asio_output(asio: AsioStatus) -> OutputStatus {
    OutputStatus {
        kind: "asio",
        asio: Some(asio),
        ..output("out-2", true, 1_800, 3)
    }
}

/// #285: the date step cost `pump_missed` +1200 on dstack ≤ 0.4.15 and the
/// ledger read clean, since only `GET /api/v1/program` carried the number.
#[test]
fn a_dante_output_loses_its_missed_blocks_and_risks_its_late_ones() {
    let dante = DanteFigures {
        pump_missed: 21,
        pump_late: 22,
        pump_overruns: 23,
        ..DanteFigures::default()
    };
    let out = OutputStatus {
        kind: "dante",
        dante: Some(dante),
        ..output("out-4", true, 1_800, 3)
    };
    let probes = output_probes(&[out]);
    assert_eq!(probes.len(), 1);
    assert_eq!(
        probes[0].counters,
        vec![
            Counter::loss(kinds::BLOCKS_DROPPED, 3),
            Counter::loss(kinds::PUMP_MISSED, 21),
            Counter::risk(kinds::PUMP_LATE, 22),
            Counter::risk(kinds::PUMP_OVERRUN, 23),
        ]
    );
}

#[test]
fn an_asio_output_loses_its_faults_risks_its_late_callbacks_and_its_host_down_is_a_state() {
    let asio = AsioStatus {
        underruns: 11,
        hard_recentres: 12,
        resets: 13,
        resyncs: 14,
        overflows: 15,
        late_callbacks: 16,
        overloads: 17,
        fast_callbacks: 18,
        reason_code: Some(HOST_DOWN_CODE),
        ..AsioStatus::default()
    };
    assert_eq!(
        output_probes(&[asio_output(asio)]),
        vec![Probe {
            path: "out:out-2".to_string(),
            detail: "Výstup out-2".to_string(),
            run: 1_800,
            gated_in: true,
            counters: vec![
                Counter::loss(kinds::BLOCKS_DROPPED, 3),
                Counter::loss(kinds::UNDERRUN, 11),
                Counter::loss(kinds::HARD_RECENTRE, 12),
                Counter::loss(kinds::RESET, 13),
                Counter::loss(kinds::RESYNC, 14),
                Counter::loss(kinds::OVERFLOW, 15),
                Counter::risk(kinds::LATE_CALLBACK, 16),
                Counter::risk(kinds::FAST_CALLBACK, 18),
                Counter::risk(kinds::OVERLOAD, 17),
            ],
            states: vec![StateFlag {
                kind: kinds::HOST_DOWN,
                severity: Severity::Loss,
                active: true,
            }],
            ..Probe::default()
        }],
        "a loop check that never ran is no path"
    );
    let waiting = AsioStatus {
        reason_code: Some("no_clock"),
        ..AsioStatus::default()
    };
    let probes = output_probes(&[asio_output(waiting)]);
    assert!(!probes[0].states[0].active, "a calm wait is no host down");
}

#[test]
fn a_loop_check_that_ran_is_its_own_path_gated_like_its_output() {
    let event = |kind, utc_ms, frames| LoopEvent {
        kind,
        utc_ms,
        ago_s: 1.0,
        frames,
    };
    let asio = || AsioStatus {
        loop_status: LoopStatus {
            state: LoopState::NoReturn,
            checked_s: 61.25,
            blind_s: 2.5,
            recent: vec![
                event(LoopEventKind::Mismatch, 5_000, 256),
                event(LoopEventKind::Dropout, 6_000, 512),
                event(LoopEventKind::Slip, 7_000, -3),
            ],
            ..LoopStatus::default()
        },
        ..AsioStatus::default()
    };
    let probes = output_probes(&[asio_output(asio())]);
    assert_eq!(probes.len(), 2, "the output, then its loop check");
    let loss = |kind, utc_ms, amount| Event {
        kind,
        severity: Severity::Loss,
        utc_ms,
        amount,
    };
    assert_eq!(
        probes[1],
        Probe {
            path: "loop:out-2".to_string(),
            detail: "Výstup out-2".to_string(),
            run: 63_750,
            gated_in: true,
            events: vec![
                loss(kinds::MISMATCH, 5_000, 256),
                loss(kinds::DROPOUT, 6_000, 512),
                loss(kinds::SLIP, 7_000, -3),
            ],
            states: vec![StateFlag {
                kind: kinds::NO_RETURN,
                severity: Severity::Risk,
                active: true,
            }],
            meters: vec![
                Meter {
                    kind: kinds::CHECKED,
                    value_ms: 61_250,
                },
                Meter {
                    kind: kinds::BLIND,
                    value_ms: 2_500,
                },
            ],
            ..Probe::default()
        }
    );
    let waiting = OutputStatus {
        state: "waiting",
        ..asio_output(asio())
    };
    let probes = output_probes(&[waiting]);
    assert_eq!(probes[1].path, "loop:out-2");
    assert!(!probes[1].gated_in, "its output waits");
}

#[test]
fn a_loop_check_ran_when_it_checks_or_has_time() {
    let status = |state, checked_s, blind_s| LoopStatus {
        state,
        checked_s,
        blind_s,
        ..LoopStatus::default()
    };
    assert!(!loop_ran(&status(LoopState::Off, 0.0, 0.0)));
    assert!(loop_ran(&status(LoopState::Searching, 0.0, 0.0)));
    assert!(loop_ran(&status(LoopState::Off, 1.0, 0.0)));
    assert!(loop_ran(&status(LoopState::Off, 0.0, 2.0)));
    let locked = LoopStatus {
        state: LoopState::Locked,
        ..LoopStatus::default()
    };
    assert!(!probes_no_return(&locked), "locked is no no-return");
}

/// The loop probe's no-return flag for `status`.
fn probes_no_return(status: &LoopStatus) -> bool {
    let asio = AsioStatus {
        loop_status: status.clone(),
        ..AsioStatus::default()
    };
    output_probes(&[asio_output(asio)])[1].states[0].active
}

/// A pipeline's health row; `underruns` = (start, mid, end) (#257).
fn snapshot(
    playlist_id: i64,
    seq: u64,
    underruns: (u64, u64, u64),
    resyncs: u64,
    unserviced: u64,
    overflows: u64,
) -> PipelineHealthSnapshot {
    PipelineHealthSnapshot {
        playlist_id,
        ndi_name: format!("SP-{playlist_id}"),
        state: PlaybackStateLabel::Playing,
        frames_submitted_total: 0,
        frames_submitted_last_5s: 0,
        observed_fps: 0.0,
        nominal_fps: 0.0,
        source_fps: 0.0,
        last_submit_ts: None,
        last_heartbeat_ts: None,
        consecutive_bad_polls: 0,
        degraded_reason: None,
        clock: crate::playback::clock_health::ClockHealth::default(),
        pacing: PacingStats {
            seq,
            resyncs,
            song_change_unserviced_slots: unserviced,
            repeats: 77,
            dropped: 88,
            ..PacingStats::default()
        },
        audio: AudioStats {
            underruns: underruns.0 + underruns.1 + underruns.2,
            underruns_start: underruns.0,
            underruns_mid: underruns.1,
            underruns_end: underruns.2,
            overflows,
            ..AudioStats::default()
        },
        lock_state: sp_core::genlock::lock_state::LockState::Unlocked,
        lock_reason: String::new(),
        transport: sp_core::playback::TransportState::Idle,
        open_failures: None,
        gpu_decoded: false,
    }
}

/// Review round 1 (🟡 3): an overflow drops the oldest audio of the
/// pipeline's grid (`audio_grid.rs` `enforce_cap`): samples lost while on
/// program. #257: its underruns by phase, each its own kind.
#[test]
fn a_pipeline_loses_its_underruns_resyncs_unserviced_slots_and_overflows_gated_on_program() {
    let on_air = BTreeSet::from([3]);
    let probes = pipeline_probes(
        &[
            snapshot(3, 500, (3, 1, 5), 2, 4, 6),
            snapshot(4, 9, (0, 0, 0), 0, 0, 0),
        ],
        &on_air,
    );
    assert_eq!(
        probes[0],
        Probe {
            path: "pipeline:3".to_string(),
            detail: "SP-3".to_string(),
            run: 500,
            gated_in: true,
            counters: vec![
                Counter::loss(kinds::UNDERRUN_START, 3),
                Counter::loss(kinds::UNDERRUN, 1),
                Counter::loss(kinds::UNDERRUN_END, 5),
                Counter::loss(kinds::RESYNC, 2),
                Counter::loss(kinds::UNSERVICED, 4),
                Counter::loss(kinds::OVERFLOW, 6),
            ],
            ..Probe::default()
        },
        "repeats and dropped frames are no fault (a 24 fps song, a 60 fps one)"
    );
    assert!(!probes[1].gated_in, "off program");
}

/// The NDI input's status, its thread running or not, with counters.
fn input(running: bool) -> NdiInputStatus {
    let mut status = ProgramBus::new().input().status(&InputSettings::default());
    status.running = running;
    status.boundaries = 900;
    status.resyncs = 2;
    status.no_source_boundaries = 7;
    status.unsupported_boundaries = 8;
    status.frames_received = 880;
    status
}

/// Review round 1 (🟡 5): the NDI input "OBS manuál" (-1) is an audio path:
/// on program, a resync and every boundary it offered as standby (silence:
/// disconnected, no video yet, an unsupported format) are losses.
#[test]
fn the_ndi_input_loses_its_resyncs_and_standby_boundaries_gated_on_program() {
    let on = BTreeSet::from([PROGRAM_INPUT_ID]);
    assert_eq!(input_probe(&input(false), &on), None, "no thread, no path");
    assert_eq!(
        input_probe(&input(true), &on),
        Some(Probe {
            path: "input".to_string(),
            detail: PROGRAM_INPUT_LABEL.to_string(),
            run: 900,
            gated_in: true,
            counters: vec![
                Counter::loss(kinds::RESYNC, 2),
                Counter::loss(kinds::NO_SOURCE, 7),
                Counter::loss(kinds::UNSUPPORTED, 8),
            ],
            ..Probe::default()
        })
    );
    let off = input_probe(&input(true), &BTreeSet::from([5])).expect("its thread runs");
    assert!(!off.gated_in, "a playlist on program");
}

/// One traced boundary, taken `late_ms` after it.
fn record(
    trace: &Arc<ProgramTrace>,
    writer: &mut crate::playback::program_trace::TraceWriter,
    k: i64,
    late_ms: i64,
) {
    let stamp = 17_900_000_000_000_000 + k * 333_333;
    let marks = BoundaryMarks {
        stamp_100ns: stamp,
        taken_100ns: stamp + late_ms * 10_000,
        fed_100ns: stamp + late_ms * 10_000,
        submit_start_100ns: stamp + late_ms * 10_000,
        submitted_100ns: stamp + late_ms * 10_000,
    };
    let shape = JobShape {
        kind: TraceKind::Source,
        live: true,
    };
    writer.record(&marks, Some(3), shape, 0);
    assert!(trace.written() > 0);
}

#[test]
fn the_trace_tally_counts_each_late_boundary_once() {
    let trace = Arc::new(ProgramTrace::with_capacity(64));
    let mut writer = trace.writer().expect("the one writer");
    let mut tally = TraceTally::default();
    assert_eq!(tally.late(&trace), 0, "nothing traced");
    record(&trace, &mut writer, 0, 1);
    record(&trace, &mut writer, 1, 50);
    record(&trace, &mut writer, 2, 1);
    assert_eq!(tally.late(&trace), 1);
    assert_eq!(tally.late(&trace), 1, "each record read once");
    record(&trace, &mut writer, 3, 40);
    record(&trace, &mut writer, 4, 60);
    assert_eq!(tally.late(&trace), 3, "two more, added");
}

#[test]
fn read_takes_every_path_from_the_bus_and_the_registry() {
    let bus = ProgramBus::new();
    bus.select_initial(3, None);
    bus.outputs().note_limiter_overs(4);
    let registry = NdiHealthRegistry::new();
    registry.update(snapshot(3, 10, (0, 2, 0), 0, 0, 0));
    registry.update(snapshot(4, 10, (0, 5, 0), 0, 0, 0));
    let mut tally = TraceTally::default();
    let probes = read(&bus, &registry, &mut tally);
    let mut paths: Vec<(&str, bool)> = probes
        .iter()
        .map(|p| (p.path.as_str(), p.gated_in))
        .collect();
    paths.sort();
    assert_eq!(
        paths,
        vec![
            ("pipeline:3", true),
            ("pipeline:4", false),
            ("program", true)
        ],
        "playlist 3 is on program"
    );
    let program = probes.iter().find(|p| p.path == "program").unwrap();
    assert!(
        program.counters.contains(&Counter::risk(kinds::LIMITED, 4)),
        "{program:?}"
    );
    let mut live = live_paths(&bus, &registry);
    live.sort();
    assert_eq!(
        live,
        vec![
            ("pipeline:3".to_string(), "SP-3".to_string()),
            ("pipeline:4".to_string(), "SP-4".to_string()),
            ("program".to_string(), "SP-program".to_string()),
        ]
    );
}
