//! #259: the wall report's published status, its log steps, the bounded
//! read of a check and the route. #254: the guard's check (an end it orders
//! runs through a recorder here), its record on the status, and the source
//! scan that keeps its one act in `guard_win.rs`.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sp_core::wall::{
    GuardEnd, GuardPhase, GuardWhy, WallFact, WallGuard, WallProblem, WallRead, WallState,
    WallStatus,
};
use tower::ServiceExt;

use super::*;
use crate::prod_env::Reader;
use crate::wall::fake::{
    ARENA_PID, FakeReader, MONITOR_PID, crash_report_window, monitor_key, monitor_times,
    nvidia_read, snv_arena_process, snv_facts, snv_nvidia, snv_times, vdd,
};
use crate::wall::guard::{ArenaProcess, EndOrder, Guard};
use crate::wall::read::{Arena, NvidiaFacts, read_facts};

const NOW: i64 = 1_791_608_681_366;

#[test]
fn a_checks_status_names_its_state_problems_facts_and_reasons() {
    let (status, problems) = status_of(&snv_facts(), WallGuard::default(), NOW);
    assert!(problems.is_empty());
    let wire = serde_json::to_value(&status).unwrap();
    assert_eq!(wire["checked"], true);
    assert_eq!(wire["state"], "ok");
    assert_eq!(wire["problems"], serde_json::json!([]));
    assert_eq!(wire["read_errors"], serde_json::json!([]));
    assert_eq!(wire["checked_at_ms"], NOW);
    assert_eq!(wire["read"]["arena_running"], true);
    assert_eq!(wire["read"]["arena_output"]["preset"], "snv surround 7680");
    assert_eq!(wire["read"]["displays"][1]["width"], 7680);
    // A healthy wall never asks NVAPI.
    assert_eq!(wire["read"]["nvidia"], "not_read");
    assert_eq!(wire["read"]["connected"], serde_json::Value::Null);
    assert_eq!(wire["read"]["grids"], serde_json::Value::Null);
    let span_gone = WallFacts {
        displays: Ok(vec![vdd()]),
        nvidia: snv_nvidia(),
        ..snv_facts()
    };
    let wire = serde_json::to_value(status_of(&span_gone, WallGuard::default(), NOW).0).unwrap();
    assert_eq!(wire["read"]["nvidia"], "read");
    assert_eq!(
        wire["read"]["grids"][0]["displays"],
        serde_json::json!(["0x80061086", "0x80061087", "0x80061083"])
    );
    let night = WallFacts {
        displays: Ok(vec![vdd()]),
        nvidia: nvidia_read(Ok(Vec::new()), Ok(Vec::new())),
        ..snv_facts()
    };
    let (status, problems) = status_of(&night, WallGuard::default(), NOW);
    assert_eq!(status.state, WallState::ScreensOff);
    assert_eq!(
        status.problems,
        problems.iter().map(WallProblem::view).collect::<Vec<_>>()
    );
    assert_eq!(status.problems[0].code, "screens_off");
}

#[test]
fn nothing_read_is_unknown_with_its_reason_once() {
    let why = "the wall read did not finish within 10s";
    let (status, problems) = status_of(&WallFacts::unread(why), WallGuard::default(), NOW);
    assert_eq!(
        status,
        WallStatus {
            checked: true,
            state: WallState::Unknown,
            problems: vec![WallProblem::Unreadable(WallFact::Arena).view()],
            read: WallRead::default(),
            read_errors: vec![why.to_string()],
            checked_at_ms: Some(NOW),
            guard: WallGuard::default(),
            songplayer_clip: Default::default(),
        }
    );
    assert_eq!(problems, [WallProblem::Unreadable(WallFact::Arena)]);
}

/// A problem state's problems.
fn missing() -> Vec<WallProblem> {
    vec![WallProblem::DisplayAbsent {
        name: "Display 1".to_string(),
        width: 7680,
        height: 1080,
        not_joined: true,
    }]
}

#[test]
fn each_change_is_logged_once() {
    let mut r = Reporter::default();
    assert_eq!(r.step(WallState::Ok, &[]), LogStep::Holds);
    assert_eq!(r.step(WallState::Ok, &[]), LogStep::Quiet);
    // Holding states change: an INFO, never a WARN.
    assert_eq!(r.step(WallState::ArenaOff, &[]), LogStep::Changed);
    assert_eq!(r.step(WallState::ArenaOff, &[]), LogStep::Quiet);
    assert_eq!(
        r.step(WallState::DisplayMissing, &missing()),
        LogStep::Problem
    );
    assert_eq!(
        r.step(WallState::DisplayMissing, &missing()),
        LogStep::Quiet
    );
    // The same state with other problems is a change.
    let unread = [WallProblem::Unreadable(WallFact::Displays)];
    let unread_nvapi = [WallProblem::Unreadable(WallFact::Nvapi)];
    assert_eq!(r.step(WallState::Unknown, &unread), LogStep::Problem);
    assert_eq!(r.step(WallState::Unknown, &unread_nvapi), LogStep::Problem);
    assert_eq!(r.step(WallState::Ok, &[]), LogStep::Cleared);
    // A first check that finds a problem warns at once.
    let mut first = Reporter::default();
    assert_eq!(
        first.step(WallState::ArenaNotDrawing, &[]),
        LogStep::Problem
    );
}

#[test]
fn a_check_publishes_into_its_store() {
    let store = WallStore::default();
    assert_eq!(store.current(), WallStatus::default());
    let mut r = Reporter::default();
    let night = WallFacts {
        displays: Ok(vec![vdd()]),
        nvidia: NvidiaFacts::NoDriver,
        ..snv_facts()
    };
    let idle = WallGuard::default;
    assert_eq!(apply(&store, &mut r, idle(), &night, 10), LogStep::Holds);
    assert_eq!(store.current(), status_of(&night, idle(), 10).0);
    assert_eq!(apply(&store, &mut r, idle(), &night, 20), LogStep::Quiet);
    assert_eq!(store.current().checked_at_ms, Some(20));
    assert_eq!(
        apply(&store, &mut r, idle(), &snv_facts(), 30),
        LogStep::Changed
    );
    assert_eq!(store.current().state, WallState::Ok);
    // #254: the guard's record goes out with the check.
    let ended = WallGuard {
        ends: 2,
        last: Some(GuardEnd {
            at_ms: 25,
            why: GuardWhy::Hung,
            pid: ARENA_PID,
            monitors: Vec::new(),
            error: None,
        }),
        phase: GuardPhase::Relaunching,
    };
    assert_eq!(
        apply(&store, &mut r, ended.clone(), &snv_facts(), 40),
        LogStep::Quiet
    );
    assert_eq!(store.current().guard, ended);
}

/// An end the glue ran with no failure, recorded nowhere.
fn nothing_failed(_: &EndOrder) -> Result<(), String> {
    Ok(())
}

/// SNV's Arena with its crash reporter's window up.
fn crashed() -> WallFacts {
    WallFacts {
        guard: Ok(vec![ArenaProcess {
            monitors: vec![monitor_key()],
            crash_report: Ok(true),
            ..snv_arena_process()
        }]),
        ..snv_facts()
    }
}

/// The box after the end: no Arena.
fn gone() -> WallFacts {
    WallFacts {
        arena: Ok(Arena::default()),
        guard: Ok(Vec::new()),
        ..snv_facts()
    }
}

#[test]
fn the_guard_ends_a_crashed_arena_once_and_the_status_names_it_until_it_runs_again() {
    let mut guard = Guard::default();
    let mut orders: Vec<EndOrder> = Vec::new();
    let mut end = |order: &EndOrder| -> Result<(), String> {
        orders.push(order.clone());
        Ok(())
    };
    let record = guard_check(&mut guard, &crashed(), NOW, &mut end);
    // Still listed at the next check (the end takes a moment): no second end.
    let again = guard_check(&mut guard, &crashed(), NOW + 5_000, &mut end);
    assert_eq!(
        orders,
        [EndOrder {
            pid: ARENA_PID,
            created_100ns: snv_times().created_100ns,
            why: GuardWhy::CrashReport,
            monitors: vec![monitor_key()],
        }]
    );
    let end_record = GuardEnd {
        at_ms: NOW,
        why: GuardWhy::CrashReport,
        pid: ARENA_PID,
        monitors: vec![MONITOR_PID],
        error: None,
    };
    let relaunching = WallGuard {
        ends: 1,
        last: Some(end_record),
        phase: GuardPhase::Relaunching,
    };
    assert_eq!((record, again), (relaunching.clone(), relaunching.clone()));
    // Gone: the status reads Arena off and names the end.
    let off = guard_check(&mut guard, &gone(), NOW + 10_000, &mut nothing_failed);
    let wire = serde_json::to_value(status_of(&gone(), off, NOW + 10_000).0).unwrap();
    assert_eq!(wire["state"], "arena_off");
    assert_eq!(wire["guard"]["ends"], 1);
    assert_eq!(wire["guard"]["phase"], "relaunching");
    assert_eq!(wire["guard"]["last"]["why"], "crash_report");
    assert_eq!(wire["guard"]["last"]["pid"], ARENA_PID);
    assert_eq!(
        wire["guard"]["last"]["monitors"],
        serde_json::json!([MONITOR_PID])
    );
    assert_eq!(wire["guard"]["last"]["at_ms"], NOW);
    assert_eq!(wire["guard"]["last"]["error"], serde_json::Value::Null);
    // A new Arena runs: the end stays on record, nothing pending.
    let relaunched = WallFacts {
        guard: Ok(vec![ArenaProcess {
            pid: ARENA_PID + 4,
            ..snv_arena_process()
        }]),
        ..snv_facts()
    };
    let back = guard_check(&mut guard, &relaunched, NOW + 15_000, &mut nothing_failed);
    assert_eq!(
        back,
        WallGuard {
            phase: GuardPhase::Idle,
            ..relaunching
        }
    );
}

#[test]
fn a_crash_report_read_from_the_box_ends_its_arena() {
    let crashed_box = FakeReader {
        processes: Ok(vec![
            (ARENA_PID, "Arena.exe".to_string()),
            (MONITOR_PID, "BugSplatMonitor.exe".to_string()),
        ]),
        parents: vec![(MONITOR_PID, ARENA_PID)],
        times: vec![
            (ARENA_PID, Ok(snv_times())),
            (MONITOR_PID, Ok(monitor_times())),
        ],
        windows: Ok(vec![(MONITOR_PID, crash_report_window())]),
        ..FakeReader::snv()
    };
    let facts = read_facts(&crashed_box);
    let mut orders = Vec::new();
    let mut end = |order: &EndOrder| -> Result<(), String> {
        orders.push(order.why);
        Ok(())
    };
    guard_check(&mut Guard::default(), &facts, NOW, &mut end);
    assert_eq!(orders, [GuardWhy::CrashReport]);
}

/// SNV's box, read through the fake.
fn snv_read() -> WallFacts {
    read_facts(&FakeReader::snv())
}

/// A read that hangs past the test's bound.
fn slow_read() -> WallFacts {
    std::thread::sleep(Duration::from_millis(500));
    snv_facts()
}

#[tokio::test]
async fn a_check_reads_within_its_bound() {
    let mut reader = Reader::named(snv_read, READ_WHAT);
    assert_eq!(
        next_check(&mut reader, Duration::from_secs(30)).await,
        snv_facts()
    );
    let mut slow = Reader::named(slow_read, READ_WHAT);
    assert_eq!(
        next_check(&mut slow, Duration::from_millis(20)).await,
        WallFacts::unread("the wall read did not finish within 20ms")
    );
}

/// The report reads the box without launching anything (a launched process
/// is what Defender scanned on PP, #233) and changes nothing on it. #254:
/// its ONE act, the guard's end of a dead Arena, lives in `guard_win.rs`
/// alone, and no wall file names Arena's clip routes (clips run only from
/// the operator's Stream Deck).
#[test]
fn the_wall_reads_spawn_no_process_and_only_the_guard_ends_one() {
    let sources = [
        ("mod.rs", include_str!("mod.rs")),
        ("read.rs", include_str!("read.rs")),
        ("resolume.rs", include_str!("resolume.rs")),
        ("decide.rs", include_str!("decide.rs")),
        ("nvapi.rs", include_str!("nvapi.rs")),
        ("win.rs", include_str!("win.rs")),
        ("guard.rs", include_str!("guard.rs")),
        ("guard_win.rs", include_str!("guard_win.rs")),
    ];
    let banned = [
        "std::process",
        "Command::new",
        "CreateProcess",
        "ShellExecute",
        "SetDisplayConfig",
        "ChangeDisplaySettings",
        "SetWindowPos",
        "PostMessage",
        "SendMessage",
        "std::fs::write",
        // NVAPI's display-changing calls (Surround grids, topology): no id
        // of one is even kept.
        "Mosaic_Set",
        "MOSAIC_SET",
        "SetCurrentTopo",
        // #254: Arena's REST routes that start or connect a clip.
        "/clips/",
        "/connect",
        "/trigger",
    ];
    for (file, text) in sources {
        for word in banned {
            assert!(!text.contains(word), "{file} uses {word}");
        }
        assert_eq!(
            text.contains("TerminateProcess"),
            file == "guard_win.rs",
            "{file}: only guard_win.rs ends a process"
        );
    }
}

#[tokio::test]
async fn the_route_answers_the_published_status() {
    let state = crate::api::routes::tests::test_state().await;
    let (published, _) = status_of(&snv_facts(), WallGuard::default(), NOW);
    global().publish(published.clone());
    let req = Request::builder()
        .uri("/api/v1/wall")
        .body(Body::empty())
        .unwrap();
    let resp = crate::api::router(state, None).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let mut answered: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    // #275: the route puts the Resolume drivers' clip fact on, stamped with
    // the server's clock at the answer (`routes_status_tests.rs` pins it).
    let clip = answered.as_object_mut().unwrap().remove("songplayer_clip");
    assert!(
        clip.is_some_and(|clip| clip["now_ms"].is_i64()),
        "the clip fact, stamped"
    );
    let mut published = serde_json::to_value(&published).unwrap();
    published.as_object_mut().unwrap().remove("songplayer_clip");
    assert_eq!(answered, published);
    global().publish(WallStatus::default());
}
