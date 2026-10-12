//! #257: the answer of `GET /api/v1/audio/faults`, built purely.

use std::collections::BTreeMap;

use super::*;

const NOW: i64 = 1_760_000_000_000;

#[test]
fn hours_are_a_whole_number_from_1_to_2160_default_24() {
    assert_eq!(parse_hours(None), Ok(24));
    assert_eq!(parse_hours(Some("1")), Ok(1));
    assert_eq!(parse_hours(Some("2160")), Ok(2160));
    assert_eq!(parse_hours(Some("0168")), Ok(168));
    for bad in [
        "0",
        "2161",
        "",
        "abc",
        "-1",
        "+5",
        "1.5",
        " 2",
        "99999999999",
    ] {
        assert_eq!(parse_hours(Some(bad)), Err(HOURS_REFUSAL), "{bad:?}");
    }
}

#[test]
fn the_window_starts_hours_before_now() {
    assert_eq!(window_start(NOW, 1), NOW - 3_600_000);
    assert_eq!(window_start(NOW, 2160), NOW - 7_776_000_000);
}

fn total(
    path: &str,
    kind: &str,
    severity: &str,
    amount: i64,
    rows: i64,
    last_ms: i64,
) -> KindTotal {
    KindTotal {
        path: path.to_string(),
        kind: kind.to_string(),
        severity: severity.to_string(),
        amount,
        rows,
        last_ms,
    }
}

fn stored(ts_ms: i64, path: &str, kind: &str, severity: &str, amount: i64) -> StoredEvent {
    StoredEvent {
        ts_ms,
        path: path.to_string(),
        kind: kind.to_string(),
        severity: severity.to_string(),
        amount,
    }
}

fn event(ts_ms: i64, kind: &str, severity: &str, amount: i64) -> FaultEvent {
    FaultEvent {
        ts_ms,
        kind: kind.to_string(),
        severity: severity.to_string(),
        amount,
    }
}

fn named(path: &str, detail: &str) -> (String, String) {
    (path.to_string(), detail.to_string())
}

/// A path that runs and has no row in the window was clean all of it.
#[test]
fn a_live_path_with_no_row_is_clean_since_the_window_starts() {
    let answer = build_answer(
        NOW,
        24,
        &[],
        Vec::new(),
        &[],
        &[named("program", "SP-program")],
    );
    assert_eq!(
        answer,
        FaultsAnswer {
            now_ms: NOW,
            from_ms: NOW - 86_400_000,
            hours: 24,
            worst: None,
            truncated: false,
            paths: vec![PathFaults {
                path: "program".to_string(),
                detail: "SP-program".to_string(),
                live: true,
                clean_since_ms: NOW - 86_400_000,
                last_loss_ms: None,
                last_risk_ms: None,
                counts: BTreeMap::new(),
                events: Vec::new(),
                loop_check: None,
            }],
        }
    );
}

#[test]
fn clean_since_is_the_last_loss_else_the_latest_start() {
    let totals = [
        total("out:a", "resync", "loss", 1, 1, NOW - 7_000),
        total("out:a", "start", "info", 1, 1, NOW - 5_000),
        total("out:a", "underrun", "loss", 4, 2, NOW - 9_000),
        total("out:b", "start", "info", 2, 2, NOW - 3_000),
        total("out:b", "late_callback", "risk", 6, 3, NOW - 1_000),
    ];
    let answer = build_answer(NOW, 1, &totals, Vec::new(), &[], &[]);
    let a = &answer.paths[0];
    assert_eq!(a.path, "out:a");
    assert_eq!(
        (a.clean_since_ms, a.last_loss_ms, a.last_risk_ms),
        (NOW - 7_000, Some(NOW - 7_000), None),
        "the NEWER of two losses; a start after it never resets it"
    );
    let b = &answer.paths[1];
    assert_eq!(
        (b.clean_since_ms, b.last_loss_ms, b.last_risk_ms),
        (NOW - 3_000, None, Some(NOW - 1_000)),
        "no loss: the latest start"
    );
    assert_eq!(answer.worst, Some("out:a".to_string()));
    assert!(!a.live && !b.live, "no path runs now");
}

#[test]
fn counts_sum_a_counters_steps_and_count_an_events_rows() {
    let totals = [
        total("loop:a", "checked", "meter", 120_000, 2, NOW - 1_000),
        total("loop:a", "blind", "meter", 2_500, 2, NOW - 1_000),
        total("loop:a", "mismatch", "loss", 768, 3, NOW - 2_000),
        total("loop:a", "no_return", "risk", 2, 2, NOW - 3_000),
        total("loop:a", "start", "info", 1, 1, NOW - 9_000),
        total("out:a", "overflow", "loss", 5_000, 2, NOW - 4_000),
    ];
    let answer = build_answer(NOW, 1, &totals, Vec::new(), &[], &[]);
    let looped = &answer.paths[0];
    assert_eq!(
        looped.counts,
        BTreeMap::from([("mismatch".to_string(), 3), ("no_return".to_string(), 2)]),
        "a mismatch's amount is its frames: three events; a start and the meters are no fault"
    );
    assert_eq!(
        looped.loop_check,
        Some(LoopSeconds {
            checked_s: 120.0,
            blind_s: 2.5,
        })
    );
    let out = &answer.paths[1];
    assert_eq!(
        out.counts,
        BTreeMap::from([("overflow".to_string(), 5_000)]),
        "a counter's amounts are its count"
    );
    assert_eq!(out.loop_check, None, "an output is no loop check");
}

#[test]
fn a_loop_check_with_no_meter_reads_zero_time() {
    let answer = build_answer(NOW, 1, &[], Vec::new(), &[], &[named("loop:a", "SG")]);
    assert_eq!(
        answer.paths[0].loop_check,
        Some(LoopSeconds {
            checked_s: 0.0,
            blind_s: 0.0,
        })
    );
}

#[test]
fn the_events_go_to_their_path_oldest_first() {
    let newest = vec![
        stored(NOW - 1_000, "out:a", "underrun", "loss", 2),
        stored(NOW - 2_000, "out:b", "late_callback", "risk", 1),
        stored(NOW - 3_000, "out:a", "start", "info", 1),
    ];
    let totals = [
        total("out:a", "start", "info", 1, 1, NOW - 3_000),
        total("out:a", "underrun", "loss", 2, 1, NOW - 1_000),
        total("out:b", "late_callback", "risk", 1, 1, NOW - 2_000),
    ];
    let answer = build_answer(NOW, 1, &totals, newest, &[], &[]);
    assert!(!answer.truncated);
    assert_eq!(
        answer.paths[0].events,
        vec![
            event(NOW - 3_000, "start", "info", 1),
            event(NOW - 1_000, "underrun", "loss", 2)
        ]
    );
    assert_eq!(
        answer.paths[1].events,
        vec![event(NOW - 2_000, "late_callback", "risk", 1)]
    );
}

#[test]
fn over_500_events_keep_the_newest_500_and_say_so() {
    let newest: Vec<StoredEvent> = (0..=MAX_EVENTS as i64)
        .map(|i| stored(NOW - i, "out:a", "underrun", "loss", 1))
        .collect();
    let answer = build_answer(NOW, 1, &[], newest, &[], &[]);
    assert!(answer.truncated);
    let events = &answer.paths[0].events;
    assert_eq!(events.len(), MAX_EVENTS);
    assert_eq!(events[0].ts_ms, NOW - 499, "the oldest kept");
    assert_eq!(events[MAX_EVENTS - 1].ts_ms, NOW, "the newest last");
    let exactly: Vec<StoredEvent> = (0..MAX_EVENTS as i64)
        .map(|i| stored(NOW - i, "out:a", "underrun", "loss", 1))
        .collect();
    assert!(!build_answer(NOW, 1, &[], exactly, &[], &[]).truncated);
}

#[test]
fn a_live_paths_own_name_wins_a_gone_path_keeps_its_stored_one() {
    let details = [named("out:a", "FOH (old)"), named("out:gone", "lv1")];
    let live = [named("out:a", "FOH")];
    let answer = build_answer(NOW, 1, &[], Vec::new(), &details, &live);
    assert_eq!(
        answer
            .paths
            .iter()
            .map(|p| (p.path.as_str(), p.detail.as_str(), p.live))
            .collect::<Vec<_>>(),
        vec![("out:a", "FOH", true), ("out:gone", "lv1", false)]
    );
}

#[test]
fn the_worst_path_has_the_most_recent_loss() {
    let totals = [
        total("out:a", "underrun", "loss", 1, 1, NOW - 5_000),
        total("out:b", "underrun", "loss", 1, 1, NOW - 1_000),
        total("out:c", "late_callback", "risk", 1, 1, NOW),
        total("pipeline:3", "underrun", "loss", 1, 1, NOW - 3_000),
    ];
    let answer = build_answer(NOW, 1, &totals, Vec::new(), &[], &[]);
    assert_eq!(
        answer.worst,
        Some("out:b".to_string()),
        "a risk is never the worst"
    );
}

#[test]
fn the_answer_serializes_with_its_loop_block_named_loop() {
    let answer = build_answer(
        NOW,
        1,
        &[],
        Vec::new(),
        &[],
        &[named("loop:a", "SG"), named("out:a", "SG")],
    );
    let json = serde_json::to_value(&answer).unwrap();
    assert_eq!(json["paths"][0]["loop"]["checked_s"], 0.0);
    assert!(json["paths"][1].get("loop").is_none(), "an output has none");
    assert_eq!(json["paths"][1]["clean_since_ms"], NOW - 3_600_000);
}
