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

fn interrupted(ts_ms: i64) -> FaultRow {
    row(ts_ms, kinds::INTERRUPTED, Severity::Loss, 1)
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

/// Review round 1 (🟡 4): a path's first tick is its baseline. An ASIO
/// output whose start-up dropped blocks (unsent before the host's first
/// session, the host's queue while the driver opens) writes no loss.
#[test]
fn a_new_path_writes_its_start_and_takes_its_counters_as_its_baseline() {
    let mut ledger = Ledger::default();
    assert_eq!(ledger.tick(&[program(5, 2, 1)], T0), vec![start(T0)]);
    assert_eq!(
        ledger.tick(&[program(9, 3, 1)], T0 + 5_000),
        vec![row(T0 + 5_000, kinds::FILLED, Severity::Loss, 1)],
        "counted from its first tick"
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

/// Review round 1 (🔵 14): an ASIO host restart reads as a rebuild (the new
/// host's `blocks_sent` starts at 0) while SongPlayer's own queue drops and
/// unsent blocks in `blocks_dropped` never reset. The rebuild is a new
/// baseline, so they are never counted twice; the break itself is one
/// `interrupted` loss when the path counted at the tick before.
#[test]
fn a_run_counter_that_went_down_is_a_rebuilt_path_its_tick_a_baseline() {
    let mut ledger = Ledger::default();
    ledger.tick(&[program(100, 5, 0)], T0);
    assert_eq!(
        ledger.tick(&[program(3, 7, 0)], T0 + 5_000),
        vec![start(T0 + 5_000), interrupted(T0 + 5_000)],
        "nothing of the rebuild's tick is counted"
    );
    assert_eq!(
        ledger.tick(&[program(9, 8, 0)], T0 + 10_000),
        vec![row(T0 + 10_000, kinds::FILLED, Severity::Loss, 1)],
        "counted from the rebuild's tick"
    );
}

#[test]
fn a_counter_that_went_down_is_a_rebuilt_path() {
    let mut ledger = Ledger::default();
    ledger.tick(&[program(100, 5, 0)], T0);
    assert_eq!(
        ledger.tick(&[program(200, 2, 0)], T0 + 5_000),
        vec![start(T0 + 5_000), interrupted(T0 + 5_000)]
    );
}

/// A pipeline (or an output) as `gated_in`, its run and its underruns.
fn gated(on: bool, run: u64, underruns: u64) -> Probe {
    Probe {
        path: "program".to_string(),
        detail: "SP-program".to_string(),
        run,
        gated_in: on,
        counters: vec![Counter::loss(kinds::UNDERRUN, underruns)],
        ..Probe::default()
    }
}

/// An output rebuilt while it waited (gated out at the tick before) broke
/// off nothing that counted; one that ran at the tick before did, even
/// when it does not run again yet (its host restarted, the driver opens).
#[test]
fn a_rebuild_is_an_interruption_only_when_the_path_counted_at_the_tick_before() {
    let mut waiting = Ledger::default();
    waiting.tick(&[gated(false, 10, 4)], T0);
    assert_eq!(
        waiting.tick(&[gated(false, 2, 1)], T0 + 5_000),
        vec![start(T0 + 5_000)]
    );
    let mut running = Ledger::default();
    running.tick(&[gated(true, 10, 4)], T0);
    assert_eq!(
        running.tick(&[gated(false, 2, 1)], T0 + 5_000),
        vec![start(T0 + 5_000), interrupted(T0 + 5_000)]
    );
}

#[test]
fn a_path_gone_and_back_starts_again() {
    let mut ledger = Ledger::default();
    ledger.tick(&[program(5, 2, 0)], T0);
    assert_eq!(ledger.tick(&[], T0 + 5_000), none());
    assert_eq!(
        ledger.tick(&[program(5, 2, 0)], T0 + 10_000),
        vec![start(T0 + 10_000)],
        "forgotten while gone: a new baseline, no break seen"
    );
}

/// Review round 1 (🟡 4, 🔵 10): a step is recorded only when the path was
/// gated in at the tick before — the tick it comes in is its baseline (a
/// playlist's off-program growth, an output's start-up), the tick it goes
/// out still counts (what grew while it was on).
#[test]
fn a_gated_path_records_a_step_only_when_it_was_gated_in_at_the_tick_before() {
    let underrun = |ts_ms, amount| row(ts_ms, kinds::UNDERRUN, Severity::Loss, amount);
    let mut ledger = Ledger::default();
    assert_eq!(ledger.tick(&[gated(false, 1, 4)], T0), vec![start(T0)]);
    assert_eq!(ledger.tick(&[gated(false, 1, 6)], T0 + 5_000), none());
    assert_eq!(
        ledger.tick(&[gated(true, 1, 9)], T0 + 10_000),
        none(),
        "the tick it comes in is its baseline"
    );
    assert_eq!(
        ledger.tick(&[gated(true, 1, 11)], T0 + 15_000),
        vec![underrun(T0 + 15_000, 2)]
    );
    assert_eq!(
        ledger.tick(&[gated(false, 1, 12)], T0 + 20_000),
        vec![underrun(T0 + 20_000, 1)],
        "gated in at the tick before"
    );
    assert_eq!(ledger.tick(&[gated(false, 1, 14)], T0 + 25_000), none());
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
        vec![start(T0)],
        "listed at first sight: the baseline"
    );
    assert_eq!(
        ledger.tick(&[vban(true, &[T0 - 700, T0 + 1_000])], T0 + 5_000),
        vec![row(T0 + 1_000, kinds::LATE_PACKET, Severity::Risk, 12_000)],
        "the first one was taken, never written"
    );
    assert_eq!(
        ledger.tick(&[vban(true, &[T0 - 700, T0 + 1_000])], T0 + 10_000),
        none(),
        "written once"
    );
}

/// Review round 1 (🔵 14): VBAN serves no packet index, so the amount (the
/// lateness, µs) tells two packets late in the same ms apart; a burst's
/// packets differ by their spacing.
#[test]
fn two_events_in_the_same_ms_are_two_rows_when_their_amounts_differ() {
    let late = |amount| Event {
        kind: kinds::LATE_PACKET,
        severity: Severity::Risk,
        utc_ms: T0 + 1_000,
        amount,
    };
    let probe = |events: Vec<Event>| Probe {
        path: "program".to_string(),
        detail: "SP-program".to_string(),
        run: 1,
        gated_in: true,
        events,
        ..Probe::default()
    };
    let mut ledger = Ledger::default();
    ledger.tick(&[probe(Vec::new())], T0);
    let both = || probe(vec![late(15_000), late(13_667)]);
    assert_eq!(
        ledger.tick(&[both()], T0 + 5_000),
        vec![
            row(T0 + 1_000, kinds::LATE_PACKET, Severity::Risk, 15_000),
            row(T0 + 1_000, kinds::LATE_PACKET, Severity::Risk, 13_667),
        ]
    );
    assert_eq!(ledger.tick(&[both()], T0 + 10_000), none());
}

#[test]
fn an_event_seen_while_gated_out_is_never_written_later() {
    let mut ledger = Ledger::default();
    ledger.tick(&[vban(false, &[])], T0);
    assert_eq!(ledger.tick(&[vban(false, &[T0 + 100])], T0 + 5_000), none());
    ledger.tick(&[vban(false, &[T0 + 100])], T0 + 10_000);
    assert_eq!(ledger.tick(&[vban(true, &[T0 + 100])], T0 + 15_000), none());
    assert_eq!(ledger.tick(&[vban(true, &[T0 + 100])], T0 + 20_000), none());
}

/// The oldest key goes once one more than [`SEEN_KEPT`] are kept: written
/// again when it is still listed, while the next one is still known.
#[test]
fn the_oldest_event_key_is_forgotten_past_the_bound() {
    let mut ledger = Ledger::default();
    let all: Vec<i64> = (0..=SEEN_KEPT as i64).collect();
    assert_eq!(
        ledger.tick(&[vban(true, &all)], T0),
        vec![start(T0)],
        "every key taken as the baseline"
    );
    assert_eq!(
        ledger.tick(&[vban(true, &[1, 0])], T0 + 5_000),
        vec![row(0, kinds::LATE_PACKET, Severity::Risk, 12_000)]
    );
}

/// The program's probe with one `host_down` state, gated in as asked.
fn host(on: bool, down: bool) -> Probe {
    Probe {
        path: "program".to_string(),
        detail: "SP-program".to_string(),
        run: 1,
        gated_in: on,
        states: vec![StateFlag {
            kind: kinds::HOST_DOWN,
            severity: Severity::Loss,
            active: down,
        }],
        ..Probe::default()
    }
}

#[test]
fn a_state_is_written_when_it_starts_to_hold() {
    let down = |ts_ms| row(ts_ms, kinds::HOST_DOWN, Severity::Loss, 1);
    let mut ledger = Ledger::default();
    assert_eq!(ledger.tick(&[host(true, false)], T0), vec![start(T0)]);
    assert_eq!(
        ledger.tick(&[host(true, true)], T0 + 5_000),
        vec![down(T0 + 5_000)]
    );
    assert_eq!(ledger.tick(&[host(true, true)], T0 + 10_000), none());
    assert_eq!(ledger.tick(&[host(true, false)], T0 + 15_000), none());
    assert_eq!(
        ledger.tick(&[host(true, true)], T0 + 20_000),
        vec![down(T0 + 20_000)]
    );
    let mut fresh = Ledger::default();
    assert_eq!(
        fresh.tick(&[host(true, true)], T0),
        vec![start(T0)],
        "holding at first sight: the baseline"
    );
    assert_eq!(fresh.tick(&[host(true, true)], T0 + 5_000), none());
}

/// Review round 1 (🟡 4): an ASIO output whose host goes down reads
/// waiting (gated out) at the same tick its `host_down` starts to hold:
/// it ran at the tick before, so the state is a loss.
#[test]
fn a_state_that_starts_as_its_path_goes_out_is_written() {
    let down = |ts_ms| row(ts_ms, kinds::HOST_DOWN, Severity::Loss, 1);
    let mut ledger = Ledger::default();
    ledger.tick(&[host(true, false)], T0);
    assert_eq!(
        ledger.tick(&[host(false, true)], T0 + 5_000),
        vec![down(T0 + 5_000)]
    );
    assert_eq!(ledger.tick(&[host(false, true)], T0 + 10_000), none());
    assert_eq!(ledger.tick(&[host(false, false)], T0 + 15_000), none());
    assert_eq!(
        ledger.tick(&[host(false, true)], T0 + 20_000),
        none(),
        "down again while it never ran in between"
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
/// first, then its start (and the break: it counted at the tick before);
/// the new run's time waits for its minute.
#[test]
fn a_meter_that_went_down_is_a_rebuilt_path_its_old_sum_written_first() {
    let mut ledger = Ledger::default();
    ledger.tick(&[looped(10, 5_000, 0)], T0);
    assert_eq!(
        ledger.tick(&[looped(20, 1_000, 0)], T0 + 5_000),
        vec![
            row(T0 + 5_000, kinds::CHECKED, Severity::Meter, 5_000),
            start(T0 + 5_000),
            interrupted(T0 + 5_000),
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
    ledger.tick(&[program(1, 0, 0)], T0);
    assert_eq!(
        ledger.tick(&[program(2, u64::MAX, 0)], T0 + 5_000),
        vec![row(T0 + 5_000, kinds::FILLED, Severity::Loss, i64::MAX)]
    );
}
