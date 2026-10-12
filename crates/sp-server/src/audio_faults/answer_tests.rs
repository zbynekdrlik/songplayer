//! #257: the answer of `GET /api/v1/audio/faults`, built purely.

use std::collections::BTreeMap;

use super::*;

const NOW: i64 = 1_760_000_000_000;
/// The ledger's 90 days.
const KEPT: i64 = 7_776_000_000;
const DAY: i64 = 86_400_000;

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

/// #257 (main session's ruling): a path the ledger has no row of at all
/// (it has not seen it yet) claims nothing: clean since now, and so is the
/// chain.
#[test]
fn a_live_path_the_ledger_has_never_seen_is_clean_since_now() {
    let answer = build_answer(
        NOW,
        24,
        &[],
        Vec::new(),
        &[],
        &[named("program", "SP-program")],
        &[],
    );
    assert_eq!(
        answer,
        FaultsAnswer {
            now_ms: NOW,
            from_ms: NOW - 86_400_000,
            hours: 24,
            kept_from_ms: NOW - KEPT,
            clean_since_ms: NOW,
            worst: None,
            truncated: false,
            paths: vec![PathFaults {
                path: "program".to_string(),
                detail: "SP-program".to_string(),
                live: true,
                clean_since_ms: NOW,
                last_loss_ms: None,
                last_risk_ms: None,
                losses: 0,
                risks: 0,
                counts: BTreeMap::new(),
                events: Vec::new(),
                loop_check: None,
            }],
        }
    );
}

fn mark(path: &str, last_loss_ms: Option<i64>, first_start_ms: Option<i64>) -> CleanMark {
    CleanMark {
        path: path.to_string(),
        last_loss_ms,
        first_start_ms,
    }
}

/// #257 (main session's ruling): a planned restart is no fault. A path is
/// clean since its newest loss the ledger keeps, BEYOND the asked window,
/// else since its FIRST start it keeps — never its latest start.
#[test]
fn clean_since_is_the_newest_loss_kept_else_the_first_start_kept() {
    let totals = [
        total("out:a", "resync", "loss", 1, 1, NOW - 7_000),
        total("out:a", "start", "info", 1, 1, NOW - 5_000),
        total("out:a", "underrun", "loss", 4, 2, NOW - 9_000),
        total("out:b", "start", "info", 2, 2, NOW - 3_000),
        total("out:b", "late_callback", "risk", 6, 3, NOW - 1_000),
        total("out:c", "start", "info", 1, 1, NOW - 2_000),
    ];
    let marks = [
        mark("out:a", Some(NOW - 7_000), Some(NOW - 9 * DAY)),
        mark("out:b", None, Some(NOW - 5 * DAY)),
        mark("out:c", Some(NOW - 3 * DAY), Some(NOW - 20 * DAY)),
    ];
    let answer = build_answer(NOW, 1, &totals, Vec::new(), &[], &[], &marks);
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
        (NOW - 5 * DAY, None, Some(NOW - 1_000)),
        "no loss: the FIRST start kept, not the restart 3 s ago"
    );
    let c = &answer.paths[2];
    assert_eq!(
        (c.clean_since_ms, c.last_loss_ms),
        (NOW - 3 * DAY, None),
        "a loss before the asked window, a restart in it"
    );
    assert_eq!(answer.worst, Some("out:a".to_string()));
    assert!(!a.live && !b.live, "no path runs now");
}

#[test]
fn a_window_loss_counts_with_the_marks_and_a_path_kept_but_unmarked_reads_all_kept() {
    let totals = [
        total("out:a", "underrun", "loss", 2, 1, NOW - 4_000),
        total("out:b", "late_callback", "risk", 1, 1, NOW - 1_000),
    ];
    let marks = [mark("out:a", Some(NOW - 8_000), Some(NOW - 2 * DAY))];
    let answer = build_answer(NOW, 1, &totals, Vec::new(), &[], &[], &marks);
    assert_eq!(
        answer.paths[0].clean_since_ms,
        NOW - 4_000,
        "the window's newer loss wins over an older mark"
    );
    assert_eq!(
        answer.paths[1].clean_since_ms,
        NOW - KEPT,
        "a path with rows but no loss or start kept ran clean for all the ledger keeps"
    );
    assert_eq!(answer.kept_from_ms, NOW - KEPT);
}

#[test]
fn the_ledger_keeps_90_days() {
    assert_eq!(kept_from(NOW), NOW - KEPT);
    assert_eq!(kept_from(KEPT), 0);
}

/// The whole chain: the newest loss of ANY path kept (a path gone since
/// counts), else the first start kept, else all kept (rows but nothing
/// marked), else now.
#[test]
fn the_chain_is_clean_since_the_newest_loss_kept_else_the_first_start() {
    let totals = [total("out:a", "start", "info", 1, 1, NOW - 2_000)];
    let marks = [
        mark("out:a", Some(NOW - 3 * DAY), Some(NOW - 30 * DAY)),
        mark("out:gone", Some(NOW - 2 * DAY), Some(NOW - 40 * DAY)),
        mark("program", None, Some(NOW - 50 * DAY)),
    ];
    let live = [named("program", "SP-program")];
    let answer = build_answer(NOW, 24, &totals, Vec::new(), &[], &live, &marks);
    assert_eq!(answer.clean_since_ms, NOW - 2 * DAY, "the gone path's loss");
    assert!(
        answer.paths.iter().all(|p| p.path != "out:gone"),
        "a gone path with no row in the window is not listed"
    );
    let window = [total("out:a", "underrun", "loss", 1, 1, NOW - 60_000)];
    let in_window = build_answer(NOW, 24, &window, Vec::new(), &[], &live, &marks);
    assert_eq!(
        in_window.clean_since_ms,
        NOW - 60_000,
        "a loss in the window"
    );
    let no_loss = [
        mark("out:a", None, Some(NOW - 30 * DAY)),
        mark("program", None, Some(NOW - 50 * DAY)),
    ];
    let starts = build_answer(NOW, 24, &totals, Vec::new(), &[], &live, &no_loss);
    assert_eq!(
        starts.clean_since_ms,
        NOW - 50 * DAY,
        "the first start kept"
    );
    let unmarked = build_answer(NOW, 24, &totals, Vec::new(), &[], &live, &[]);
    assert_eq!(unmarked.clean_since_ms, NOW - KEPT, "rows, nothing marked");
    let nothing = build_answer(NOW, 24, &[], Vec::new(), &[], &live, &[]);
    assert_eq!(nothing.clean_since_ms, NOW, "no row at all");
}

/// #257 lane 2: the dashboard's count is the window's loss / risk rows,
/// every kind of each summed (a counter's step of 5 is one row, as is an
/// event); a start or a meter is neither.
#[test]
fn losses_and_risks_count_the_windows_rows_per_severity() {
    let totals = [
        total("out:a", "underrun", "loss", 5, 2, NOW - 4_000),
        total("out:a", "overflow", "loss", 4_096, 1, NOW - 3_000),
        total("out:a", "late_callback", "risk", 7, 3, NOW - 2_000),
        total("out:a", "start", "info", 1, 1, NOW - 9_000),
        total("loop:a", "no_return", "risk", 1, 1, NOW - 1_000),
        total("loop:a", "checked", "meter", 60_000, 4, NOW - 1_000),
    ];
    let answer = build_answer(NOW, 1, &totals, Vec::new(), &[], &[], &[]);
    let rows: Vec<(&str, i64, i64)> = answer
        .paths
        .iter()
        .map(|p| (p.path.as_str(), p.losses, p.risks))
        .collect();
    assert_eq!(rows, [("loop:a", 0, 1), ("out:a", 3, 3)]);
    let json = serde_json::to_value(&answer).unwrap();
    assert_eq!(json["paths"][1]["losses"], 3);
    assert_eq!(json["paths"][1]["risks"], 3);
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
    let answer = build_answer(NOW, 1, &totals, Vec::new(), &[], &[], &[]);
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
    let answer = build_answer(NOW, 1, &[], Vec::new(), &[], &[named("loop:a", "SG")], &[]);
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
    let answer = build_answer(NOW, 1, &totals, newest, &[], &[], &[]);
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
    let answer = build_answer(NOW, 1, &[], newest, &[], &[], &[]);
    assert!(answer.truncated);
    let events = &answer.paths[0].events;
    assert_eq!(events.len(), MAX_EVENTS);
    assert_eq!(events[0].ts_ms, NOW - 499, "the oldest kept");
    assert_eq!(events[MAX_EVENTS - 1].ts_ms, NOW, "the newest last");
    let exactly: Vec<StoredEvent> = (0..MAX_EVENTS as i64)
        .map(|i| stored(NOW - i, "out:a", "underrun", "loss", 1))
        .collect();
    assert!(!build_answer(NOW, 1, &[], exactly, &[], &[], &[]).truncated);
}

#[test]
fn a_live_paths_own_name_wins_a_gone_path_keeps_its_stored_one() {
    let details = [named("out:a", "FOH (old)"), named("out:gone", "lv1")];
    let live = [named("out:a", "FOH")];
    let answer = build_answer(NOW, 1, &[], Vec::new(), &details, &live, &[]);
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
    let answer = build_answer(NOW, 1, &totals, Vec::new(), &[], &[], &[]);
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
        &[],
    );
    let json = serde_json::to_value(&answer).unwrap();
    assert_eq!(json["paths"][0]["loop"]["checked_s"], 0.0);
    assert!(json["paths"][1].get("loop").is_none(), "an output has none");
    assert_eq!(
        json["paths"][1]["clean_since_ms"], NOW,
        "a path never seen claims nothing"
    );
    assert_eq!(json["kept_from_ms"], NOW - KEPT);
    assert_eq!(json["clean_since_ms"], NOW);
}
