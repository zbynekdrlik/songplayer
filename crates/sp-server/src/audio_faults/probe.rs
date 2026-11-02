//! #257: every audio path as the ledger reads it, from the in-process
//! status structs the API serves (`GET /api/v1/program`,
//! `GET /api/v1/ndi/health`), never over HTTP. The builders are pure; only
//! [`read`] and [`live_paths`] touch the bus and the registry.
//!
//! The paths and what each counts (the design on #257, its severities):
//!
//! - `program` (`SP-program`): `filled` (a real source's boundary filled,
//!   `health.filled_owned`), `resync` (`health.resyncs_owned`: one over a
//!   real source's boundaries) and `coalesced` (a boundary the sender's full
//!   queue dropped: never served, VBAN has no gap detection) are losses;
//!   `late_dropped`, `vban_feed_late`
//!   (`health.timing.vban_feed_late_over_budget`), `limited` (the limiter
//!   outside a fade) and `ndi_late` (boundaries taken over a slot late, from
//!   the trace) are risks. Never gated.
//! - `out:<id>`, every enabled output but the post-deploy probe's own
//!   (`e2e-…`): `blocks_dropped` (loss). VBAN: `blocks_substituted`,
//!   `send_error` (losses), each packet over 10 ms late (`late_packet`, a
//!   risk event). ASIO: `underrun`, `hard_recentre`, `reset`, `resync`,
//!   `overflow` (frames) and the host going down (`host_down`, a state) are
//!   losses; `late_callback`, `fast_callback` and `overload` are risks.
//!   Gated in while the output runs (`state` `running`): its start-up, a
//!   driver that opens, a `no_clock` wait are no fault.
//! - `loop:<id>`, an ASIO output whose loop check ran: each `mismatch`,
//!   `dropout`, `slip` event (loss, its frames), `no_return` (a state, a
//!   risk: the check cannot tell), and the `checked` / `blind` meters;
//!   gated like its output.
//! - `input` (the NDI input "OBS manuál", while its thread runs): `resync`,
//!   `no_source` and `unsupported` (boundaries it offered as standby, i.e.
//!   silence) are losses, gated in while it is on program.
//! - `pipeline:<playlist id>`: `underrun_start` / `underrun` /
//!   `underrun_end` (#257: a song's audio late at its start, short
//!   mid-song, cut before its end; the block a song's audio ends in is no
//!   underrun), `resync`, `unserviced`, `overflow` (its audio grid dropped
//!   its oldest samples) are losses, gated in while the playlist is on
//!   program.
//!
//! On program ([`on_program`]) = the program's selected source and the
//! outgoing side of a running fade (mixed until it ends, up to ~10.5 s).

use std::collections::BTreeSet;

use sp_core::config::PROGRAM_INPUT_ID;
use sp_core::dante_status::DanteFigures;

use super::model::{Counter, Event, Meter, Probe, Severity, StateFlag, kinds};
use crate::playback::asio_loop::{LoopEventKind, LoopState, LoopStatus};
use crate::playback::asio_out::AsioStatus;
use crate::playback::audio_out::{OutputStatus, STATE_RUNNING};
use crate::playback::ndi_health::{NdiHealthRegistry, PipelineHealthSnapshot};
use crate::playback::ndi_input::{InputSettings, NdiInputStatus};
use crate::playback::program_bus::{ProgramBus, ProgramStatus};
use crate::playback::program_trace::{MinuteLog, ProgramTrace};
use crate::playback::vban_out::VbanStatus;

/// The program's path.
pub const PROGRAM_PATH: &str = "program";

/// The NDI input's path.
pub const INPUT_PATH: &str = "input";

/// The id prefix of the post-deploy probe's own throwaway outputs (the
/// 96 kHz probe, `e2e-96k`): the ledger never watches them, so they are in
/// no answer and never the worst path.
pub const PROBE_OUTPUT_PREFIX: &str = "e2e-";

/// A VBAN packet later than this (µs) is a `late_packet`.
pub const LATE_PACKET_US: u64 = 10_000;

/// An ASIO output's waiting reason while its host process is down
/// (`asio_state::Reason::HostDown`'s code).
pub const HOST_DOWN_CODE: &str = "host_down";

/// Seconds as whole ms (a NaN or a negative reads 0).
fn ms(seconds: f64) -> u64 {
    (seconds * 1000.0) as u64
}

/// The sources whose audio is on `SP-program` now: the selected one and
/// the outgoing side of a running fade.
pub fn on_program(status: &ProgramStatus) -> BTreeSet<i64> {
    let fading_out = status.transition.active.and_then(|w| w.from);
    status.source.into_iter().chain(fading_out).collect()
}

/// The program's probe: its health, the limiter's engagements outside a
/// fade and the trace's late boundaries.
pub fn program_probe(status: &ProgramStatus, limiter_overs: u64, ndi_late: u64) -> Probe {
    let h = &status.health;
    Probe {
        path: PROGRAM_PATH.to_string(),
        detail: status.ndi_name.to_string(),
        run: h.submitted,
        gated_in: true,
        counters: vec![
            Counter::loss(kinds::FILLED, h.filled_owned),
            Counter::loss(kinds::RESYNC, h.resyncs_owned),
            Counter::risk(kinds::LATE_DROPPED, h.late_dropped),
            Counter::loss(kinds::COALESCED, h.coalesced),
            Counter::risk(kinds::VBAN_FEED_LATE, h.timing.vban_feed_late_over_budget),
            Counter::risk(kinds::LIMITED, limiter_overs),
            Counter::risk(kinds::NDI_LATE, ndi_late),
        ],
        ..Probe::default()
    }
}

/// A VBAN output's own faults.
fn vban_into(probe: &mut Probe, vban: &VbanStatus) {
    probe.counters.extend([
        Counter::loss(kinds::BLOCKS_SUBSTITUTED, vban.blocks_substituted),
        Counter::loss(kinds::SEND_ERROR, vban.send_errors),
    ]);
    let late = vban
        .late_events
        .iter()
        .filter(|e| e.late_us > LATE_PACKET_US);
    probe.events.extend(late.map(|e| Event {
        kind: kinds::LATE_PACKET,
        severity: Severity::Risk,
        utc_ms: e.utc_ms,
        amount: i64::try_from(e.late_us).unwrap_or(i64::MAX),
    }));
}

/// An ASIO output's own faults.
fn asio_into(probe: &mut Probe, asio: &AsioStatus) {
    probe.counters.extend([
        Counter::loss(kinds::UNDERRUN, asio.underruns),
        Counter::loss(kinds::HARD_RECENTRE, asio.hard_recentres),
        Counter::loss(kinds::RESET, asio.resets),
        Counter::loss(kinds::RESYNC, asio.resyncs),
        Counter::loss(kinds::OVERFLOW, asio.overflows),
        Counter::risk(kinds::LATE_CALLBACK, asio.late_callbacks),
        Counter::risk(kinds::FAST_CALLBACK, asio.fast_callbacks),
        Counter::risk(kinds::OVERLOAD, asio.overloads),
    ]);
    probe.states.push(StateFlag {
        kind: kinds::HOST_DOWN,
        severity: Severity::Loss,
        active: asio.reason_code == Some(HOST_DOWN_CODE),
    });
}

/// A Dante output's own faults, beside the ones its seam reports as ASIO.
fn dante_into(probe: &mut Probe, dante: &DanteFigures) {
    probe.counters.extend([
        Counter::loss(kinds::PUMP_MISSED, dante.pump_missed),
        Counter::risk(kinds::PUMP_LATE, dante.pump_late),
        Counter::risk(kinds::PUMP_OVERRUN, dante.pump_overruns),
    ]);
}

/// Whether an output's loop check ran (it checks now, or it has time).
pub fn loop_ran(status: &LoopStatus) -> bool {
    status.state != LoopState::Off || status.checked_s + status.blind_s > 0.0
}

/// The kind of a loop event.
fn loop_kind(kind: LoopEventKind) -> &'static str {
    match kind {
        LoopEventKind::Mismatch => kinds::MISMATCH,
        LoopEventKind::Dropout => kinds::DROPOUT,
        LoopEventKind::Slip => kinds::SLIP,
    }
}

/// An ASIO output's loop check (`loop:<id>`), gated like its output.
fn loop_probe(output: &OutputStatus, status: &LoopStatus, gated_in: bool) -> Probe {
    let (checked, blind) = (ms(status.checked_s), ms(status.blind_s));
    Probe {
        path: format!("loop:{}", output.id),
        detail: output.name.clone(),
        run: checked + blind,
        gated_in,
        events: status
            .recent
            .iter()
            .map(|e| Event {
                kind: loop_kind(e.kind),
                severity: Severity::Loss,
                utc_ms: e.utc_ms,
                amount: e.frames,
            })
            .collect(),
        states: vec![StateFlag {
            kind: kinds::NO_RETURN,
            severity: Severity::Risk,
            active: status.state == LoopState::NoReturn,
        }],
        meters: vec![
            Meter {
                kind: kinds::CHECKED,
                value_ms: checked,
            },
            Meter {
                kind: kinds::BLIND,
                value_ms: blind,
            },
        ],
        ..Probe::default()
    }
}

/// Whether the ledger watches `output`: enabled, and not one of the
/// post-deploy probe's own ([`PROBE_OUTPUT_PREFIX`]).
fn watched(output: &OutputStatus) -> bool {
    output.enabled && !output.id.starts_with(PROBE_OUTPUT_PREFIX)
}

/// Every watched output's probe (`out:<id>`), each followed by its loop
/// check's when it ran (`loop:<id>`); gated in while it runs.
pub fn output_probes(outputs: &[OutputStatus]) -> Vec<Probe> {
    let mut probes = Vec::new();
    for output in outputs.iter().filter(|o| watched(o)) {
        let running = output.state == STATE_RUNNING;
        let mut probe = Probe {
            path: format!("out:{}", output.id),
            detail: output.name.clone(),
            run: output.blocks_sent,
            gated_in: running,
            counters: vec![Counter::loss(kinds::BLOCKS_DROPPED, output.blocks_dropped)],
            ..Probe::default()
        };
        if let Some(vban) = &output.vban {
            vban_into(&mut probe, vban);
        }
        if let Some(asio) = &output.asio {
            asio_into(&mut probe, asio);
        }
        if let Some(dante) = &output.dante {
            dante_into(&mut probe, dante);
        }
        probes.push(probe);
        if let Some(asio) = &output.asio
            && loop_ran(&asio.loop_status)
        {
            probes.push(loop_probe(output, &asio.loop_status, running));
        }
    }
    probes
}

/// The NDI input's probe (`input`) while its thread runs; `None` while it
/// does not (off Windows, no NDI SDK). Gated in while it is on program.
pub fn input_probe(input: &NdiInputStatus, on_program: &BTreeSet<i64>) -> Option<Probe> {
    input.running.then(|| Probe {
        path: INPUT_PATH.to_string(),
        detail: input.label.to_string(),
        run: input.boundaries,
        gated_in: on_program.contains(&PROGRAM_INPUT_ID),
        counters: vec![
            Counter::loss(kinds::RESYNC, input.resyncs),
            Counter::loss(kinds::NO_SOURCE, input.no_source_boundaries),
            Counter::loss(kinds::UNSUPPORTED, input.unsupported_boundaries),
        ],
        ..Probe::default()
    })
}

/// Every playlist pipeline's probe (`pipeline:<id>`), gated in while it is
/// on program.
pub fn pipeline_probes(snaps: &[PipelineHealthSnapshot], on_program: &BTreeSet<i64>) -> Vec<Probe> {
    snaps
        .iter()
        .map(|s| Probe {
            path: format!("pipeline:{}", s.playlist_id),
            detail: s.ndi_name.clone(),
            run: s.pacing.seq,
            gated_in: on_program.contains(&s.playlist_id),
            counters: vec![
                Counter::loss(kinds::UNDERRUN_START, s.audio.underruns_start),
                Counter::loss(kinds::UNDERRUN, s.audio.underruns_mid),
                Counter::loss(kinds::UNDERRUN_END, s.audio.underruns_end),
                Counter::loss(kinds::RESYNC, s.pacing.resyncs),
                Counter::loss(kinds::UNSERVICED, s.pacing.song_change_unserviced_slots),
                Counter::loss(kinds::OVERFLOW, s.audio.overflows),
            ],
            ..Probe::default()
        })
        .collect()
}

/// The `SP-program` boundaries the trace saw taken over a slot late, since
/// the tally was made (each record read once).
#[derive(Debug, Default)]
pub struct TraceTally {
    minutes: MinuteLog,
    late: u64,
}

impl TraceTally {
    /// The late boundaries so far, the records written since the last call
    /// added.
    pub fn late(&mut self, trace: &ProgramTrace) -> u64 {
        if let Some(summary) = self.minutes.minute(trace) {
            self.late += summary.counts.late;
        }
        self.late
    }
}

/// The NDI input's counters (the settings `status` reports are not read).
fn input_status(bus: &ProgramBus) -> NdiInputStatus {
    bus.input().status(&InputSettings::default())
}

/// Every audio path now: the program, the outputs (and their loop checks),
/// the NDI input, the playlist pipelines.
pub fn read(bus: &ProgramBus, registry: &NdiHealthRegistry, tally: &mut TraceTally) -> Vec<Probe> {
    let ndi_late = tally.late(bus.trace());
    let status = bus.status();
    let on_program = on_program(&status);
    let program = program_probe(&status, bus.outputs().limiter_overs(), ndi_late);
    let mut probes = vec![program];
    probes.extend(output_probes(&bus.outputs().status()));
    probes.extend(input_probe(&input_status(bus), &on_program));
    probes.extend(pipeline_probes(&registry.snapshots(), &on_program));
    probes
}

/// The paths that run now, with their names (the API lists them even with
/// no row in its window).
pub fn live_paths(bus: &ProgramBus, registry: &NdiHealthRegistry) -> Vec<(String, String)> {
    let mut probes = vec![program_probe(&bus.status(), 0, 0)];
    probes.extend(output_probes(&bus.outputs().status()));
    probes.extend(input_probe(&input_status(bus), &BTreeSet::new()));
    probes.extend(pipeline_probes(&registry.snapshots(), &BTreeSet::new()));
    probes.into_iter().map(|p| (p.path, p.detail)).collect()
}

#[cfg(test)]
#[path = "probe_tests.rs"]
mod tests;
