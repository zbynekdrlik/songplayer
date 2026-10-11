//! #233: the production-environment guard — the edition rule, the verdict
//! table, the firewall and Defender reads, the published status, the log
//! steps, the bounded reader and both routes.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sp_core::prod_env::{
    EnvProblem, EnvironmentStatus, FirewallProfile, FirewallState, Unreadable,
};
use tower::ServiceExt;

use super::*;

/// A production box: IoT Enterprise LTSC, Defender real-time off, every
/// firewall profile off.
fn production() -> EnvFacts {
    EnvFacts {
        edition_id: Some("IoTEnterpriseS".to_string()),
        product_name: Some("Windows 10 IoT Enterprise LTSC 2024".to_string()),
        defender_realtime: Some(false),
        firewall: [Some(false); 3],
        read_errors: Vec::new(),
    }
}

fn with_edition(edition: &str) -> EnvFacts {
    EnvFacts {
        edition_id: Some(edition.to_string()),
        ..production()
    }
}

// ---- the edition rule (the owner's ruling, 11.10.2026) ----

#[test]
fn every_ltsc_edition_and_iot_enterprise_are_allowed() {
    for e in [
        "EnterpriseS",
        "EnterpriseSN",
        "IoTEnterpriseS",
        "IoTEnterpriseSK",
    ] {
        assert!(is_ltsc(e), "{e} is LTSC");
        assert!(is_allowed_edition(e), "{e} is allowed");
    }
    assert!(!is_ltsc("IoTEnterprise"), "IoT Enterprise is no LTSC");
    assert!(
        is_allowed_edition("IoTEnterprise"),
        "IoT Enterprise is allowed"
    );
    // As Windows writes them or not, padded or not.
    assert!(is_ltsc("iotenterprises"));
    assert!(is_ltsc(" EnterpriseS "));
    assert!(is_allowed_edition("IOTENTERPRISE"));
    assert!(is_allowed_edition(" IoTEnterprise "));
}

#[test]
fn every_other_edition_is_not_allowed() {
    for e in [
        "Enterprise",
        "Professional",
        "Core",
        "Education",
        "IoTEnterpriseK",
        "EnterpriseSEval",
        "ServerStandard",
        "",
    ] {
        assert!(!is_ltsc(e), "{e:?} is no LTSC");
        assert!(!is_allowed_edition(e), "{e:?} is not allowed");
    }
}

// ---- the verdict ----

#[test]
fn a_production_box_holds() {
    let v = verdict(&production());
    assert_eq!(
        v,
        EnvVerdict {
            ok: true,
            ltsc: Some(true),
            edition_allowed: Some(true),
            problems: Vec::new(),
        }
    );
}

#[test]
fn iot_enterprise_holds_and_is_no_ltsc() {
    let v = verdict(&with_edition("IoTEnterprise"));
    assert_eq!(
        v,
        EnvVerdict {
            ok: true,
            ltsc: Some(false),
            edition_allowed: Some(true),
            problems: Vec::new(),
        }
    );
}

#[test]
fn an_edition_that_is_not_allowed_is_named() {
    for e in ["Enterprise", "Professional", "Core"] {
        let v = verdict(&with_edition(e));
        assert!(!v.ok, "{e}");
        assert_eq!(
            (v.ltsc, v.edition_allowed),
            (Some(false), Some(false)),
            "{e}"
        );
        assert_eq!(
            v.problems,
            [EnvProblem::EditionNotAllowed(e.to_string())],
            "{e}"
        );
    }
    // Named trimmed, as compared.
    let v = verdict(&with_edition(" Professional "));
    assert_eq!(
        v.problems,
        [EnvProblem::EditionNotAllowed("Professional".to_string())]
    );
}

#[test]
fn an_edition_that_cannot_be_read_is_a_problem_never_fine() {
    for edition in [None, Some(String::new()), Some("  ".to_string())] {
        let v = verdict(&EnvFacts {
            edition_id: edition.clone(),
            ..production()
        });
        assert!(!v.ok, "{edition:?}");
        assert_eq!((v.ltsc, v.edition_allowed), (None, None), "{edition:?}");
        assert_eq!(
            v.problems,
            [EnvProblem::Unreadable(Unreadable::Edition)],
            "{edition:?}"
        );
    }
}

#[test]
fn defender_on_or_unread_is_a_problem() {
    let on = verdict(&EnvFacts {
        defender_realtime: Some(true),
        ..production()
    });
    assert!(!on.ok);
    assert_eq!(on.problems, [EnvProblem::DefenderRealtimeOn]);
    let unread = verdict(&EnvFacts {
        defender_realtime: None,
        ..production()
    });
    assert!(!unread.ok);
    assert_eq!(
        unread.problems,
        [EnvProblem::Unreadable(Unreadable::Defender)]
    );
}

#[test]
fn each_firewall_profile_on_or_unread_is_named_by_its_profile() {
    for (i, profile) in FirewallProfile::ALL.into_iter().enumerate() {
        let mut firewall = [Some(false); 3];
        firewall[i] = Some(true);
        let on = verdict(&EnvFacts {
            firewall,
            ..production()
        });
        assert!(!on.ok, "{profile:?}");
        assert_eq!(on.problems, [EnvProblem::FirewallOn(profile)]);
        firewall[i] = None;
        let unread = verdict(&EnvFacts {
            firewall,
            ..production()
        });
        assert!(!unread.ok, "{profile:?}");
        assert_eq!(
            unread.problems,
            [EnvProblem::Unreadable(Unreadable::Firewall(profile))]
        );
    }
}

#[test]
fn every_problem_is_listed_in_order() {
    let v = verdict(&EnvFacts {
        edition_id: Some("Professional".to_string()),
        product_name: None,
        defender_realtime: Some(true),
        firewall: [Some(true), None, Some(true)],
        read_errors: Vec::new(),
    });
    assert!(!v.ok);
    assert_eq!(
        v.problems,
        [
            EnvProblem::EditionNotAllowed("Professional".to_string()),
            EnvProblem::DefenderRealtimeOn,
            EnvProblem::FirewallOn(FirewallProfile::Domain),
            EnvProblem::Unreadable(Unreadable::Firewall(FirewallProfile::Private)),
            EnvProblem::FirewallOn(FirewallProfile::Public),
        ]
    );
    let nothing = verdict(&EnvFacts::unread("the check did not finish within 30s"));
    assert_eq!(
        nothing.problems,
        [
            EnvProblem::Unreadable(Unreadable::Edition),
            EnvProblem::Unreadable(Unreadable::Defender),
            EnvProblem::Unreadable(Unreadable::Firewall(FirewallProfile::Domain)),
            EnvProblem::Unreadable(Unreadable::Firewall(FirewallProfile::Private)),
            EnvProblem::Unreadable(Unreadable::Firewall(FirewallProfile::Public)),
        ]
    );
}

// ---- the firewall: the policy wins over the local value ----

#[test]
fn a_firewall_policy_wins_over_the_local_value() {
    use RegDword::{Absent, Failed, Value};
    let d = FirewallProfile::Domain;
    assert_eq!(firewall_on(d, Value(0), Value(1)), Ok(false));
    assert_eq!(firewall_on(d, Value(1), Value(0)), Ok(true));
    assert_eq!(firewall_on(d, Value(2), Absent), Ok(true));
    assert_eq!(firewall_on(d, Value(0), Failed(5)), Ok(false));
}

#[test]
fn with_no_policy_the_local_value_decides() {
    use RegDword::{Absent, Value};
    let p = FirewallProfile::Private;
    assert_eq!(firewall_on(p, Absent, Value(0)), Ok(false));
    assert_eq!(firewall_on(p, Absent, Value(1)), Ok(true));
    assert_eq!(firewall_on(p, Absent, Value(7)), Ok(true));
}

#[test]
fn a_firewall_that_cannot_be_read_says_why() {
    use RegDword::{Absent, Failed, Value};
    assert_eq!(
        firewall_on(FirewallProfile::Domain, Failed(5), Value(0)),
        Err("the domain firewall policy could not be read (error 5)".to_string())
    );
    assert_eq!(
        firewall_on(FirewallProfile::Private, Absent, Absent),
        Err("the private firewall setting is missing".to_string())
    );
    assert_eq!(
        firewall_on(FirewallProfile::Public, Absent, Failed(1630)),
        Err("the public firewall setting could not be read (error 1630)".to_string())
    );
}

// ---- Defender ----

#[test]
fn the_wmi_status_codes_are_wmis() {
    assert_eq!(WBEM_E_INVALID_NAMESPACE as u32, 0x8004_100E);
    assert_eq!(WBEM_E_INVALID_CLASS as u32, 0x8004_1010);
}

#[test]
fn defenders_real_time_protection_is_read_from_its_status() {
    assert_eq!(defender_realtime(Ok(Some(true))), Ok(true));
    assert_eq!(defender_realtime(Ok(Some(false))), Ok(false));
}

#[test]
fn defender_not_installed_is_off() {
    assert_eq!(defender_realtime(Err(WBEM_E_INVALID_NAMESPACE)), Ok(false));
    assert_eq!(defender_realtime(Err(WBEM_E_INVALID_CLASS)), Ok(false));
}

#[test]
fn a_defender_status_that_cannot_be_read_says_why() {
    assert_eq!(
        defender_realtime(Ok(None)),
        Err("Defender's status names no real-time protection".to_string())
    );
    // E_ACCESSDENIED
    assert_eq!(
        defender_realtime(Err(0x8007_0005_u32 as i32)),
        Err("Defender's status could not be read (0x80070005)".to_string())
    );
    // WBEM_S_TIMEDOUT, a success code the glue hands over as a failure.
    assert_eq!(
        defender_realtime(Err(0x0004_0004)),
        Err("Defender's status could not be read (0x00040004)".to_string())
    );
}

// ---- the published status ----

#[test]
fn off_windows_the_status_holds_and_says_why() {
    let (status, problems) = status_of(&Check::NotWindows, 1_000);
    assert_eq!(
        status,
        EnvironmentStatus {
            checked: true,
            ok: true,
            note: Some(NOT_WINDOWS.to_string()),
            checked_at_ms: Some(1_000),
            ..EnvironmentStatus::default()
        }
    );
    assert!(problems.is_empty());
}

#[test]
fn the_status_carries_every_fact_and_problem() {
    let facts = EnvFacts {
        edition_id: Some("Professional".to_string()),
        product_name: Some("Windows 11 Pro".to_string()),
        defender_realtime: Some(true),
        firewall: [Some(true), None, Some(false)],
        read_errors: vec!["the private firewall setting is missing".to_string()],
    };
    let (status, problems) = status_of(&Check::Read(facts), 2_000);
    let expected_problems = vec![
        EnvProblem::EditionNotAllowed("Professional".to_string()),
        EnvProblem::DefenderRealtimeOn,
        EnvProblem::FirewallOn(FirewallProfile::Domain),
        EnvProblem::Unreadable(Unreadable::Firewall(FirewallProfile::Private)),
    ];
    assert_eq!(problems, expected_problems);
    assert_eq!(
        status,
        EnvironmentStatus {
            checked: true,
            ok: false,
            note: None,
            edition_id: Some("Professional".to_string()),
            product_name: Some("Windows 11 Pro".to_string()),
            ltsc: Some(false),
            edition_allowed: Some(false),
            defender_realtime: Some(true),
            firewall: FirewallState {
                domain: Some(true),
                private: None,
                public: Some(false),
            },
            problems: expected_problems.iter().map(EnvProblem::view).collect(),
            read_errors: vec!["the private firewall setting is missing".to_string()],
            checked_at_ms: Some(2_000),
        }
    );
    let (held, none) = status_of(&Check::Read(production()), 3_000);
    assert!(held.ok);
    assert!(none.is_empty());
    assert_eq!(held.ltsc, Some(true));
    assert_eq!(held.edition_allowed, Some(true));
}

// ---- what a check logs ----

#[test]
fn each_change_is_logged_once() {
    let edition = |e: &str| EnvProblem::EditionNotAllowed(e.to_string());
    let mut r = Reporter::default();
    assert_eq!(r.step(&[]), LogStep::Holds);
    assert_eq!(r.step(&[]), LogStep::Quiet);
    assert_eq!(r.step(&[EnvProblem::DefenderRealtimeOn]), LogStep::Problems);
    assert_eq!(r.step(&[EnvProblem::DefenderRealtimeOn]), LogStep::Quiet);
    let two = [
        EnvProblem::DefenderRealtimeOn,
        EnvProblem::FirewallOn(FirewallProfile::Public),
    ];
    assert_eq!(r.step(&two), LogStep::Problems);
    assert_eq!(r.step(&two), LogStep::Quiet);
    assert_eq!(r.step(&[edition("Core")]), LogStep::Problems);
    // The same code, another edition: a change.
    assert_eq!(r.step(&[edition("Professional")]), LogStep::Problems);
    assert_eq!(r.step(&[]), LogStep::Cleared);
    assert_eq!(r.step(&[]), LogStep::Quiet);
}

#[test]
fn a_first_check_with_problems_logs_them() {
    let mut r = Reporter::default();
    assert_eq!(r.step(&[EnvProblem::DefenderRealtimeOn]), LogStep::Problems);
    assert_eq!(r.step(&[]), LogStep::Cleared);
}

#[test]
fn a_check_publishes_its_status_and_logs_each_change() {
    let store = EnvStore::default();
    assert_eq!(store.current(), EnvironmentStatus::default());
    let mut r = Reporter::default();
    let bad = Check::Read(EnvFacts {
        defender_realtime: Some(true),
        ..production()
    });
    assert_eq!(apply(&store, &mut r, &bad, 10), LogStep::Problems);
    assert_eq!(store.current(), status_of(&bad, 10).0);
    assert!(!store.current().ok);
    assert_eq!(apply(&store, &mut r, &bad, 20), LogStep::Quiet);
    assert_eq!(store.current().checked_at_ms, Some(20));
    let good = Check::Read(production());
    assert_eq!(apply(&store, &mut r, &good, 30), LogStep::Cleared);
    assert_eq!(store.current(), status_of(&good, 30).0);
    assert!(store.current().ok);
}

// ---- the bounded reader ----

static GATE: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());
static GATED_READS: AtomicUsize = AtomicUsize::new(0);

/// A read that waits until the gate opens (only the reader test uses it).
fn gated_read() -> EnvFacts {
    GATED_READS.fetch_add(1, Ordering::SeqCst);
    let (open, opened) = &GATE;
    let mut is_open = open.lock().unwrap_or_else(|e| e.into_inner());
    while !*is_open {
        is_open = opened.wait(is_open).unwrap_or_else(|e| e.into_inner());
    }
    production()
}

/// Opens the gate when dropped, so a failing test never leaves the read
/// blocked (the runtime's drop waits for its blocking tasks).
struct OpenGate;

impl Drop for OpenGate {
    fn drop(&mut self) {
        let (open, opened) = &GATE;
        *open.lock().unwrap_or_else(|e| e.into_inner()) = true;
        opened.notify_all();
    }
}

#[tokio::test]
async fn a_hung_read_is_unreadable_and_waited_for_never_started_again() {
    let gate = OpenGate;
    let mut reader = Reader::new(gated_read);
    // The read cannot finish while the gate is shut: the bound passes.
    let first = reader.next(Duration::from_millis(50)).await;
    assert_eq!(
        first,
        EnvFacts::unread("the check did not finish within 50ms")
    );
    let second = reader.next(Duration::from_millis(50)).await;
    assert_eq!(second, first);
    // Released: the next call takes that same read's facts. Had the second
    // call started another read, two would have run by now (each counts
    // before it waits at the gate).
    drop(gate);
    let third = reader.next(Duration::from_secs(60)).await;
    assert_eq!(third, production());
    assert_eq!(
        GATED_READS.load(Ordering::SeqCst),
        1,
        "one read, waited for twice"
    );
    // Done with it: the next call reads again.
    let fourth = reader.next(Duration::from_secs(60)).await;
    assert_eq!(fourth, production());
    assert_eq!(GATED_READS.load(Ordering::SeqCst), 2);
}

fn panicking_read() -> EnvFacts {
    panic!("the registry read panicked");
}

#[tokio::test]
async fn a_read_that_panics_is_unreadable() {
    let mut reader = Reader::new(panicking_read);
    let facts = reader.next(Duration::from_secs(60)).await;
    assert_eq!(facts.edition_id, None);
    assert_eq!(facts.read_errors.len(), 1);
    assert!(
        facts.read_errors[0].starts_with("the check failed: "),
        "{:?}",
        facts.read_errors
    );
    assert!(!verdict(&facts).ok);
}

fn quick_read() -> EnvFacts {
    with_edition("IoTEnterprise")
}

#[tokio::test]
async fn a_check_reads_through_the_reader_or_says_not_windows() {
    assert_eq!(
        next_check(None, Duration::from_secs(60)).await,
        Check::NotWindows
    );
    let mut reader = Reader::new(quick_read);
    assert_eq!(
        next_check(Some(&mut reader), Duration::from_secs(60)).await,
        Check::Read(with_edition("IoTEnterprise"))
    );
}

// ---- the routes (the process's store, through the real router) ----

/// Serializes the tests that publish into the process's store.
static GLOBAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn get_json(state: crate::AppState, uri: &str) -> serde_json::Value {
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let resp = crate::api::router(state, None).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn both_routes_answer_the_published_status() {
    let _serial = GLOBAL.lock().await;
    let state = crate::api::routes::tests::test_state().await;
    let check = Check::Read(EnvFacts {
        firewall: [Some(false), Some(true), Some(false)],
        ..production()
    });
    let (published, _) = status_of(&check, 1_791_608_681_366);
    global().publish(published.clone());
    let expected = serde_json::to_value(&published).unwrap();
    assert_eq!(expected["ok"], false);
    assert_eq!(
        expected["problems"],
        serde_json::json!([{
            "code": "firewall_on:private",
            "text_sk": "brána firewall je zapnutá (súkromný profil)",
        }])
    );
    assert_eq!(
        get_json(state.clone(), "/api/v1/environment").await,
        expected
    );
    let status = get_json(state, "/api/v1/status").await;
    assert_eq!(status["environment"], expected);
    global().publish(EnvironmentStatus::default());
}
