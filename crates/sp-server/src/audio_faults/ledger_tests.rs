//! #257: the ledger's rows, tick by tick, pinned exactly.

use super::{Ledger, METER_FLUSH_MS, SEEN_KEPT};
use crate::audio_faults::model::{
    Counter, Event, FaultRow, Meter, Probe, Severity, StateFlag, kinds,
};

const T0: i64 = 1_760_000_000_000;

fn row(ts_ms: i64, kind: &'static str, severity: Severity, amount: i64) -> FaultRow {
    FaultRow {
        ts_ms,
        path: "program".to_string(),
        kind,
        severity,
        amount,
        detail: "SP-program".to_string(),
    }
}

/// No row.
fn none() -> Vec<FaultRow> {
    Vec::new()
}

fn start(ts_ms: i64) -> FaultRow {
    row(ts_ms, kinds::START, Severity::Info, 1)
}

/// The program's probe with `filled` (a loss) and `coalesced` (a risk).
fn program(run: u64, filled: u64, coalesced: u64) -> Probe {
    Probe {
        path: "program".to_string(),
        detail: "SP-program".to_string(),
        run,
        gated_in: true,
        counters: vec![
            Counter::loss(kinds::FILLED, filled),
            Counter::risk(kinds::COALESCED, coalesced),
        ],
        ..Probe::default()
    }
}

#[test]
fn a_new_path_writes_its_start_and_counts_its_counters_from_zero() {
    let mut ledger = Ledger::default();
    assert_eq!(
        ledger.tick(&[program(5, 2, 0)], T0),
        vec![start(T0), row(T0, kinds::FILLED, Severity::Loss, 2)]
    );
}

#[test]
fn a_steady_path_writes_nothing() {
    let mut ledger = Ledger::default();
    ledger.tick(&[program(5, 2, 1)], T0);
    assert_eq!(ledger.tick(&[program(9, 2, 1)], T0 + 5_000), none());
}

#[test]
fn a_counter_that_grew_writes_its_step() {
    let mut ledger = Ledger::default();
    ledger.tick(&[program(5, 2, 4)], T0);
    assert_eq!(
        ledger.tick(&[program(9, 5, 7)], T0 + 5_000),
        vec![
            row(T0 + 5_000, kinds::FILLED, Severity::Loss, 3),
            row(T0 + 5_000, kinds::COALESCED, Severity::Risk, 3),
        ]
    );
}

#[test]
fn a_run_counter_that_went_down_is_a_rebuilt_path() {
    let mut ledger = Ledger::default();
    ledger.tick(&[program(100, 5, 0)], T0);
    assert_eq!(
        ledger.tick(&[program(3, 7, 0)], T0 + 5_000),
        vec![
            start(T0 + 5_000),
            row(T0 + 5_000, kinds::FILLED, Severity::Loss, 7)
        ],
        "its counters count from 0 again"
    );
}

#[test]
fn a_counter_that_went_down_is_a_rebuilt_path() {
    let mut ledger = Ledger::default();
    ledger.tick(&[program(100, 5, 0)], T0);
    assert_eq!(
        ledger.tick(&[program(200, 2, 0)], T0 + 5_000),
        vec![
            start(T0 + 5_000),
            row(T0 + 5_000, kinds::FILLED, Severity::Loss, 2)
        ]
    );
}

#[test]
fn a_path_gone_and_back_starts_again() {
    let mut ledger = Ledger::default();
    ledger.tick(&[program(5, 2, 0)], T0);
    assert_eq!(ledger.tick(&[], T0 + 5_000), none());
    assert_eq!(
        ledger.tick(&[program(5, 2, 0)], T0 + 10_000),
        vec![
            start(T0 + 10_000),
            row(T0 + 10_000, kinds::FILLED, Severity::Loss, 2)
        ],
        "forgotten while gone"
    );
}

/// A pipeline's underruns: recorded while it is on program, or was at the
/// tick before; off program on both ticks they are taken, never recorded.
#[test]
fn a_gated_out_path_records_only_next_to_a_gated_in_tick() {
    let pipeline = |on: bool, underruns: u64| Probe {
        path: "pipeline:3".to_string(),
        detail: "SP-slow".to_string(),
        run: 1,
        gated_in: on,
        counters: vec![Counter::loss(kinds::UNDERRUN, underruns)],
        ..Probe::default()
    };
    let underrun = |ts_ms: i64, amount: i64| FaultRow {
        ts_ms,
        path: "pipeline:3".to_string(),
        kind: kinds::UNDERRUN,
        severity: Severity::Loss,
        amount,
        detail: "SP-slow".to_string(),
    };
    let mut ledger = Ledger::default();
    let first = ledger.tick(&[pipeline(false, 4)], T0);
    assert_eq!(first.len(), 1, "its start only: {first:?}");
    assert_eq!(ledger.tick(&[pipeline(false, 6)], T0 + 5_000), none());
    assert_eq!(
        ledger.tick(&[pipeline(true, 9)], T0 + 10_000),
        vec![underrun(T0 + 10_000, 3)]
    );
    assert_eq!(
        ledger.tick(&[pipeline(false, 10)], T0 + 15_000),
        vec![underrun(T0 + 15_000, 1)],
        "on program at the tick before"
    );
    assert_eq!(ledger.tick(&[pipeline(false, 14)], T0 + 20_000), none());
}

fn vban(on: bool, utc_ms: &[i64]) -> Probe {
    Probe {
        path: "program".to_string(),
        detail: "SP-program".to_string(),
        run: 1,
        gated_in: on,
        events: utc_ms
            .iter()
            .map(|&utc_ms| Event {
                kind: kinds::LATE_PACKET,
                severity: Severity::Risk,
                utc_ms,
                amount: 12_000,
            })
            .collect(),
        ..Probe::default()
    }
}

#[test]
fn an_event_is_written_once_at_its_own_time() {
    let mut ledger = Ledger::default();
    assert_eq!(
        ledger.tick(&[vban(true, &[T0 - 700])], T0),
        vec![
            start(T0),
            row(T0 - 700, kinds::LATE_PACKET, Severity::Risk, 12_000)
        ]
    );
    assert_eq!(
        ledger.tick(&[vban(true, &[T0 - 700, T0 + 1_000])], T0 + 5_000),
        vec![row(T0 + 1_000, kinds::LATE_PACKET, Severity::Risk, 12_000)],
        "the first one is still listed: written already"
    );
}

#[test]
fn an_event_seen_while_gated_out_is_never_written_later() {
    let mut ledger = Ledger::default();
    ledger.tick(&[vban(false, &[])], T0);
    assert_eq!(ledger.tick(&[vban(false, &[T0 + 100])], T0 + 5_000), none());
    ledger.tick(&[vban(false, &[T0 + 100])], T0 + 10_000);
    assert_eq!(ledger.tick(&[vban(true, &[T0 + 100])], T0 + 15_000), none());
}

/// The oldest key goes once one more than [`SEEN_KEPT`] are kept: written
/// again when it is still listed, while the next one is still known.
#[test]
fn the_oldest_event_key_is_forgotten_past_the_bound() {
    let mut ledger = Ledger::default();
    let all: Vec<i64> = (0..=SEEN_KEPT as i64).collect();
    let rows = ledger.tick(&[vban(true, &all)], T0);
    assert_eq!(rows.len(), SEEN_KEPT + 2, "the start and every event");
    assert_eq!(
        ledger.tick(&[vban(true, &[1, 0])], T0 + 5_000),
        vec![row(0, kinds::LATE_PACKET, Severity::Risk, 12_000)]
    );
}

#[test]
fn a_state_is_written_when_it_starts_to_hold() {
    let host = |down: bool| Probe {
        path: "program".to_string(),
        detail: "SP-program".to_string(),
        run: 1,
        gated_in: true,
        states: vec![StateFlag {
            kind: kinds::HOST_DOWN,
            severity: Severity::Loss,
            active: down,
        }],
        ..Probe::default()
    };
    let down = |ts_ms| row(ts_ms, kinds::HOST_DOWN, Severity::Loss, 1);
    let mut ledger = Ledger::default();
    assert_eq!(ledger.tick(&[host(false)], T0), vec![start(T0)]);
    assert_eq!(
        ledger.tick(&[host(true)], T0 + 5_000),
        vec![down(T0 + 5_000)]
    );
    assert_eq!(ledger.tick(&[host(true)], T0 + 10_000), none());
    assert_eq!(ledger.tick(&[host(false)], T0 + 15_000), none());
    assert_eq!(
        ledger.tick(&[host(true)], T0 + 20_000),
        vec![down(T0 + 20_000)]
    );
    let mut fresh = Ledger::default();
    assert_eq!(
        fresh.tick(&[host(true)], T0),
        vec![start(T0), down(T0)],
        "holding at first sight"
    );
}

fn looped(run: u64, checked: u64, blind: u64) -> Probe {
    Probe {
        path: "program".to_string(),
        detail: "SP-program".to_string(),
        run,
        gated_in: true,
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

#[test]
fn a_meter_is_summed_and_written_once_a_minute() {
    let mut ledger = Ledger::default();
    assert_eq!(ledger.tick(&[looped(1, 1_000, 0)], T0), vec![start(T0)]);
    assert_eq!(
        ledger.tick(&[looped(2, 3_000, 0)], T0 + METER_FLUSH_MS - 1),
        none()
    );
    assert_eq!(
        ledger.tick(&[looped(3, 4_000, 0)], T0 + METER_FLUSH_MS),
        vec![row(
            T0 + METER_FLUSH_MS,
            kinds::CHECKED,
            Severity::Meter,
            4_000
        )],
        "the minute's sum, from the start; a blind time of 0 writes no row"
    );
    assert_eq!(
        ledger.tick(&[looped(4, 4_000, 0)], T0 + 2 * METER_FLUSH_MS),
        none(),
        "nothing grew"
    );
    assert_eq!(
        ledger.tick(&[looped(5, 5_500, 250)], T0 + 3 * METER_FLUSH_MS),
        vec![
            row(T0 + 3 * METER_FLUSH_MS, kinds::BLIND, Severity::Meter, 250),
            row(
                T0 + 3 * METER_FLUSH_MS,
                kinds::CHECKED,
                Severity::Meter,
                1_500
            ),
        ]
    );
}

/// A meter that went down is a rebuilt path: the old run's sum goes out
/// first, then its start; the new run's time waits for its minute.
#[test]
fn a_meter_that_went_down_is_a_rebuilt_path_its_old_sum_written_first() {
    let mut ledger = Ledger::default();
    ledger.tick(&[looped(10, 5_000, 0)], T0);
    assert_eq!(
        ledger.tick(&[looped(20, 1_000, 0)], T0 + 5_000),
        vec![
            row(T0 + 5_000, kinds::CHECKED, Severity::Meter, 5_000),
            start(T0 + 5_000),
        ]
    );
    assert_eq!(
        ledger.tick(&[looped(30, 1_200, 0)], T0 + 5_000 + METER_FLUSH_MS),
        vec![row(
            T0 + 5_000 + METER_FLUSH_MS,
            kinds::CHECKED,
            Severity::Meter,
            1_200
        )],
        "counted from 0 since the restart"
    );
}

#[test]
fn a_step_over_i64_is_written_as_its_max() {
    let mut ledger = Ledger::default();
    assert_eq!(
        ledger.tick(&[program(1, u64::MAX, 0)], T0),
        vec![start(T0), row(T0, kinds::FILLED, Severity::Loss, i64::MAX)]
    );
}
