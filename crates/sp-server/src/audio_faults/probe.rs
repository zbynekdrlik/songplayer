//! #257: every audio path as the ledger reads it, from the in-process
//! status structs the API serves (`GET /api/v1/program`,
//! `GET /api/v1/ndi/health`), never over HTTP. The builders are pure; only
//! [`read`] and [`live_paths`] touch the bus and the registry.
//!
//! The paths and what each counts (the design on #257, its severities):
//!
//! - `program` (`SP-program`): `filled` (a real source's boundary filled,
//!   `health.filled_owned`) and `resync` are losses; `late_dropped`,
//!   `coalesced`, `vban_feed_late` (`health.timing.vban_feed_late_over_budget`),
//!   `limited` (the limiter outside a fade) and `ndi_late` (boundaries taken
//!   over a slot late, from the trace) are risks.
//! - `out:<id>`, every enabled output: `blocks_dropped` (loss). VBAN:
//!   `blocks_substituted`, `send_error` (losses), each packet over 10 ms late
//!   (`late_packet`, a risk event). ASIO: `underrun`, `hard_recentre`,
//!   `reset`, `resync`, `overflow` (frames) and the host going down
//!   (`host_down`, a state) are losses; `late_callback` is a risk.
//! - `loop:<id>`, an ASIO output whose loop check ran: each `mismatch`,
//!   `dropout`, `slip` event (loss, its frames), `no_return` (a state, a
//!   risk: the check cannot tell), and the `checked` / `blind` meters.
//! - `pipeline:<playlist id>`: `underrun`, `resync`, `unserviced` (losses),
//!   recorded only while the playlist is on program (the authority's on-air
//!   set).

use std::collections::BTreeSet;

use super::model::{Counter, Event, Meter, Probe, Severity, StateFlag, kinds};
use crate::playback::asio_loop::{LoopEventKind, LoopState, LoopStatus};
use crate::playback::asio_out::AsioStatus;
use crate::playback::audio_out::OutputStatus;
use crate::playback::ndi_health::{NdiHealthRegistry, PipelineHealthSnapshot};
use crate::playback::program_bus::{ProgramBus, ProgramStatus};
use crate::playback::program_on_air::on_air_set;
use crate::playback::program_trace::{MinuteLog, ProgramTrace};
use crate::playback::vban_out::VbanStatus;

/// The program's path.
pub const PROGRAM_PATH: &str = "program";

/// A VBAN packet later than this (µs) is a `late_packet`.
pub const LATE_PACKET_US: u64 = 10_000;

/// An ASIO output's waiting reason while its host process is down
/// (`asio_state::Reason::HostDown`'s code).
pub const HOST_DOWN_CODE: &str = "host_down";

/// Seconds as whole ms (a NaN or a negative reads 0).
fn ms(seconds: f64) -> u64 {
    (seconds * 1000.0) as u64
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
            Counter::loss(kinds::RESYNC, h.resyncs),
            Counter::risk(kinds::LATE_DROPPED, h.late_dropped),
            Counter::risk(kinds::COALESCED, h.coalesced),
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
    ]);
    probe.states.push(StateFlag {
        kind: kinds::HOST_DOWN,
        severity: Severity::Loss,
        active: asio.reason_code == Some(HOST_DOWN_CODE),
    });
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

/// An ASIO output's loop check (`loop:<id>`).
fn loop_probe(output: &OutputStatus, status: &LoopStatus) -> Probe {
    let (checked, blind) = (ms(status.checked_s), ms(status.blind_s));
    Probe {
        path: format!("loop:{}", output.id),
        detail: output.name.clone(),
        run: checked + blind,
        gated_in: true,
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

/// Every enabled output's probe (`out:<id>`), each followed by its loop
/// check's when it ran (`loop:<id>`).
pub fn output_probes(outputs: &[OutputStatus]) -> Vec<Probe> {
    let mut probes = Vec::new();
    for output in outputs.iter().filter(|o| o.enabled) {
        let mut probe = Probe {
            path: format!("out:{}", output.id),
            detail: output.name.clone(),
            run: output.blocks_sent,
            gated_in: true,
            counters: vec![Counter::loss(kinds::BLOCKS_DROPPED, output.blocks_dropped)],
            ..Probe::default()
        };
        if let Some(vban) = &output.vban {
            vban_into(&mut probe, vban);
        }
        if let Some(asio) = &output.asio {
            asio_into(&mut probe, asio);
        }
        probes.push(probe);
        if let Some(asio) = &output.asio
            && loop_ran(&asio.loop_status)
        {
            probes.push(loop_probe(output, &asio.loop_status));
        }
    }
    probes
}

/// Every playlist pipeline's probe (`pipeline:<id>`), gated in while it is
/// in `on_air`.
pub fn pipeline_probes(snaps: &[PipelineHealthSnapshot], on_air: &BTreeSet<i64>) -> Vec<Probe> {
    snaps
        .iter()
        .map(|s| Probe {
            path: format!("pipeline:{}", s.playlist_id),
            detail: s.ndi_name.clone(),
            run: s.pacing.seq,
            gated_in: on_air.contains(&s.playlist_id),
            counters: vec![
                Counter::loss(kinds::UNDERRUN, s.audio.underruns),
                Counter::loss(kinds::RESYNC, s.pacing.resyncs),
                Counter::loss(kinds::UNSERVICED, s.pacing.song_change_unserviced_slots),
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

/// Every audio path now: the program, the outputs (and their loop checks),
/// the playlist pipelines.
pub fn read(bus: &ProgramBus, registry: &NdiHealthRegistry, tally: &mut TraceTally) -> Vec<Probe> {
    let ndi_late = tally.late(bus.trace());
    let program = program_probe(&bus.status(), bus.outputs().limiter_overs(), ndi_late);
    let mut probes = vec![program];
    probes.extend(output_probes(&bus.outputs().status()));
    let on_air = on_air_set(&bus.on_air_now());
    probes.extend(pipeline_probes(&registry.snapshots(), &on_air));
    probes
}

/// The paths that run now, with their names (the API lists them even with
/// no row in its window).
pub fn live_paths(bus: &ProgramBus, registry: &NdiHealthRegistry) -> Vec<(String, String)> {
    let mut probes = vec![program_probe(&bus.status(), 0, 0)];
    probes.extend(output_probes(&bus.outputs().status()));
    probes.extend(pipeline_probes(&registry.snapshots(), &BTreeSet::new()));
    probes.into_iter().map(|p| (p.path, p.detail)).collect()
}

#[cfg(test)]
#[path = "probe_tests.rs"]
mod tests;
