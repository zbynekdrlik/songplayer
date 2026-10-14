//! #259: the wall report's published status, its log steps, the bounded
//! read of a check and the route.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sp_core::wall::{WallFact, WallProblem, WallRead, WallState, WallStatus};
use tower::ServiceExt;

use super::*;
use crate::prod_env::Reader;
use crate::wall::fake::{FakeReader, nvidia_read, snv_facts, snv_nvidia, vdd};
use crate::wall::read::{NvidiaFacts, read_facts};

const NOW: i64 = 1_791_608_681_366;

#[test]
fn a_checks_status_names_its_state_problems_facts_and_reasons() {
    let (status, problems) = status_of(&snv_facts(), NOW);
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
    let wire = serde_json::to_value(status_of(&span_gone, NOW).0).unwrap();
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
    let (status, problems) = status_of(&night, NOW);
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
    let (status, problems) = status_of(&WallFacts::unread(why), NOW);
    assert_eq!(
        status,
        WallStatus {
            checked: true,
            state: WallState::Unknown,
            problems: vec![WallProblem::Unreadable(WallFact::Arena).view()],
            read: WallRead::default(),
            read_errors: vec![why.to_string()],
            checked_at_ms: Some(NOW),
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
    assert_eq!(apply(&store, &mut r, &night, 10), LogStep::Holds);
    assert_eq!(store.current(), status_of(&night, 10).0);
    assert_eq!(apply(&store, &mut r, &night, 20), LogStep::Quiet);
    assert_eq!(store.current().checked_at_ms, Some(20));
    assert_eq!(apply(&store, &mut r, &snv_facts(), 30), LogStep::Changed);
    assert_eq!(store.current().state, WallState::Ok);
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
/// is what Defender scanned on PP, #233) and changes nothing on it.
#[test]
fn the_wall_reads_spawn_no_process_and_set_nothing() {
    let sources = [
        ("mod.rs", include_str!("mod.rs")),
        ("read.rs", include_str!("read.rs")),
        ("resolume.rs", include_str!("resolume.rs")),
        ("decide.rs", include_str!("decide.rs")),
        ("nvapi.rs", include_str!("nvapi.rs")),
        ("win.rs", include_str!("win.rs")),
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
    ];
    for (file, text) in sources {
        for word in banned {
            assert!(!text.contains(word), "{file} uses {word}");
        }
    }
}

#[tokio::test]
async fn the_route_answers_the_published_status() {
    let state = crate::api::routes::tests::test_state().await;
    let (published, _) = status_of(&snv_facts(), NOW);
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
    let answered: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(answered, serde_json::to_value(&published).unwrap());
    global().publish(WallStatus::default());
}
