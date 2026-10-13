//! #233: the production-environment guard — the edition rule, the verdict
//! table, the published status, the log steps and both routes (the raw
//! reads: `prod_env_read_tests.rs`; the bounded reader:
//! `prod_env_reader_tests.rs`).

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
        build_number: Some(26100),
        defender_realtime: Some(false),
        defender_passive: Some(false),
        defender_mode: Some("Normal".to_string()),
        defender_antivirus_enabled: Some(true),
        antivirus_on: Some(Vec::new()),
        firewall: [Some(false); 3],
        audio: audio::production_example(),
        read_errors: Vec::new(),
    }
}

/// Production but for the edition and the build.
fn with_edition(edition: &str, build: Option<u32>) -> EnvFacts {
    EnvFacts {
        edition_id: Some(edition.to_string()),
        build_number: build,
        ..production()
    }
}

fn not_allowed(edition: &str, build: Option<u32>) -> EnvProblem {
    EnvProblem::EditionNotAllowed {
        edition: edition.to_string(),
        build,
    }
}

// ---- the edition rule (the owner's rulings, 11.10.2026) ----

#[test]
fn every_ltsc_edition_is_allowed_at_any_build() {
    for e in [
        "EnterpriseS",
        "EnterpriseSN",
        "IoTEnterpriseS",
        "IoTEnterpriseSK",
    ] {
        assert!(is_ltsc(e), "{e} is LTSC");
        assert!(!is_iot_enterprise(e), "{e} is no plain IoT Enterprise");
        for build in [None, Some(17763), Some(19045), Some(26100)] {
            assert_eq!(edition_allowed(e, build), Some(true), "{e} {build:?}");
        }
    }
    // As Windows writes them or not, padded or not.
    assert!(is_ltsc("iotenterprises"));
    assert!(is_ltsc(" EnterpriseS "));
}

#[test]
fn iot_enterprise_is_allowed_on_windows_11_only() {
    for e in [
        "IoTEnterprise",
        "IoTEnterpriseK",
        "IOTENTERPRISE",
        " iotenterprisek ",
    ] {
        assert!(!is_ltsc(e), "{e} is no LTSC");
        assert!(is_iot_enterprise(e), "{e}");
        assert_eq!(
            edition_allowed(e, Some(WINDOWS_11_BUILD)),
            Some(true),
            "{e}"
        );
        assert_eq!(edition_allowed(e, Some(22631)), Some(true), "{e}");
        assert_eq!(edition_allowed(e, Some(21_999)), Some(false), "{e}");
        assert_eq!(edition_allowed(e, Some(19045)), Some(false), "{e}");
        assert_eq!(edition_allowed(e, None), None, "{e}: the build decides");
    }
    assert_eq!(WINDOWS_11_BUILD, 22_000);
}

#[test]
fn every_other_edition_is_not_allowed_at_any_build() {
    for e in [
        "Enterprise",
        "Professional",
        "Core",
        "Education",
        "EnterpriseSEval",
        "ServerStandard",
        "",
    ] {
        assert!(!is_ltsc(e), "{e:?} is no LTSC");
        assert!(!is_iot_enterprise(e), "{e:?} is no IoT Enterprise");
        for build in [None, Some(22631)] {
            assert_eq!(edition_allowed(e, build), Some(false), "{e:?} {build:?}");
        }
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
fn windows_11_iot_enterprise_holds_and_is_no_ltsc() {
    let v = verdict(&with_edition("IoTEnterprise", Some(22631)));
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
fn windows_10_iot_enterprise_is_not_allowed_and_names_its_build() {
    let v = verdict(&with_edition("IoTEnterprise", Some(19045)));
    assert!(!v.ok);
    assert_eq!((v.ltsc, v.edition_allowed), (Some(false), Some(false)));
    assert_eq!(v.problems, [not_allowed("IoTEnterprise", Some(19045))]);
}

#[test]
fn iot_enterprise_with_no_build_cannot_be_verified() {
    let v = verdict(&with_edition("IoTEnterpriseK", None));
    assert!(!v.ok);
    assert_eq!((v.ltsc, v.edition_allowed), (Some(false), None));
    assert_eq!(v.problems, [EnvProblem::Unreadable(Unreadable::Build)]);
    // An LTSC edition needs no build.
    assert!(verdict(&with_edition("IoTEnterpriseS", None)).ok);
}

#[test]
fn an_edition_that_is_not_allowed_is_named() {
    for e in ["Enterprise", "Professional", "Core"] {
        let v = verdict(&with_edition(e, Some(22631)));
        assert!(!v.ok, "{e}");
        assert_eq!(
            (v.ltsc, v.edition_allowed),
            (Some(false), Some(false)),
            "{e}"
        );
        assert_eq!(v.problems, [not_allowed(e, Some(22631))], "{e}");
    }
    // Named trimmed, as compared.
    let v = verdict(&with_edition(" Professional ", None));
    assert_eq!(v.problems, [not_allowed("Professional", None)]);
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

/// Review finding 2: Defender passive, or another antivirus product on.
#[test]
fn a_scanning_defender_or_antivirus_is_a_problem() {
    let passive = verdict(&EnvFacts {
        defender_passive: Some(true),
        defender_mode: Some("EDR Block Mode".to_string()),
        ..production()
    });
    assert!(!passive.ok);
    assert_eq!(
        passive.problems,
        [EnvProblem::DefenderPassive("EDR Block Mode".to_string())]
    );
    let both = verdict(&EnvFacts {
        defender_realtime: Some(true),
        defender_passive: Some(true),
        defender_mode: None,
        ..production()
    });
    assert_eq!(
        both.problems,
        [
            EnvProblem::DefenderRealtimeOn,
            EnvProblem::DefenderPassive(String::new()),
        ]
    );
    let antivirus = verdict(&EnvFacts {
        antivirus_on: Some(vec!["ESET Security".to_string(), "Other".to_string()]),
        ..production()
    });
    assert!(!antivirus.ok);
    assert_eq!(
        antivirus.problems,
        [
            EnvProblem::AntivirusOn("ESET Security".to_string()),
            EnvProblem::AntivirusOn("Other".to_string()),
        ]
    );
}

#[test]
fn a_defender_mode_or_antivirus_list_not_read_is_a_problem() {
    let no_passive = verdict(&EnvFacts {
        defender_passive: None,
        ..production()
    });
    assert!(!no_passive.ok);
    assert_eq!(
        no_passive.problems,
        [EnvProblem::Unreadable(Unreadable::Defender)]
    );
    let no_list = verdict(&EnvFacts {
        antivirus_on: None,
        ..production()
    });
    assert!(!no_list.ok);
    assert_eq!(
        no_list.problems,
        [EnvProblem::Unreadable(Unreadable::Antivirus)]
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
        ..production()
    });
    assert!(!v.ok);
    assert_eq!(
        v.problems,
        [
            not_allowed("Professional", Some(26100)),
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
            EnvProblem::Unreadable(Unreadable::Antivirus),
            EnvProblem::Unreadable(Unreadable::Firewall(FirewallProfile::Domain)),
            EnvProblem::Unreadable(Unreadable::Firewall(FirewallProfile::Private)),
            EnvProblem::Unreadable(Unreadable::Firewall(FirewallProfile::Public)),
            EnvProblem::Unreadable(Unreadable::SoundGrid),
            EnvProblem::Unreadable(Unreadable::WindowsUpdate),
            EnvProblem::Unreadable(Unreadable::Mmcss),
        ]
    );
}

/// #255: the audio profile's problems come after the #233 ones, and make
/// the box fail like any other.
#[test]
fn the_audio_profile_s_problems_follow_the_firewall_s() {
    let mut audio = audio::production_example();
    audio.no_auto_update = Some(false);
    let v = verdict(&EnvFacts {
        firewall: [None, Some(false), Some(false)],
        audio,
        ..production()
    });
    assert!(!v.ok);
    assert_eq!(
        v.problems,
        [
            EnvProblem::Unreadable(Unreadable::Firewall(FirewallProfile::Domain)),
            EnvProblem::AutoUpdateOn,
        ]
    );
    let mut cx = audio::production_example();
    if let audio::SoundGridFacts::Adapter { nic, .. } = &mut cx.soundgrid {
        nic.kmdf_library_version = audio::Reading::Value("1.33".to_string());
    }
    let v = verdict(&EnvFacts {
        audio: cx,
        ..production()
    });
    assert!(!v.ok);
    assert_eq!(
        v.problems,
        [EnvProblem::SoundGridNicNetAdapterCx {
            nic: "soundgrid-pcie".to_string()
        }]
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
        build_number: Some(22631),
        defender_realtime: Some(true),
        defender_passive: Some(true),
        defender_mode: Some("Passive Mode".to_string()),
        defender_antivirus_enabled: Some(false),
        antivirus_on: Some(vec!["ESET Security".to_string()]),
        firewall: [Some(true), None, Some(false)],
        audio: audio::production_example(),
        read_errors: vec!["the private firewall setting is missing".to_string()],
    };
    let (status, problems) = status_of(&Check::Read(facts), 2_000);
    let expected_problems = vec![
        not_allowed("Professional", Some(22631)),
        EnvProblem::DefenderRealtimeOn,
        EnvProblem::DefenderPassive("Passive Mode".to_string()),
        EnvProblem::AntivirusOn("ESET Security".to_string()),
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
            build_number: Some(22631),
            ltsc: Some(false),
            edition_allowed: Some(false),
            defender_realtime: Some(true),
            defender_passive: Some(true),
            defender_mode: Some("Passive Mode".to_string()),
            defender_antivirus_enabled: Some(false),
            antivirus_on: Some(vec!["ESET Security".to_string()]),
            firewall: FirewallState {
                domain: Some(true),
                private: None,
                public: Some(false),
            },
            audio: audio::view(&audio::production_example()),
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
    // #255: the audio profile and its report-only notes; they never make
    // the box fail.
    assert_eq!(
        held.audio.soundgrid_port_mac.as_deref(),
        Some("6c2408e59c91")
    );
    assert_eq!(
        held.audio
            .soundgrid_nic
            .as_ref()
            .and_then(|n| n.service.as_deref()),
        Some("rt640x64")
    );
    assert_eq!(held.audio.notes.len(), 2);
    // Off Windows nothing is read, so no note either.
    let (off, _) = status_of(&Check::NotWindows, 4_000);
    assert_eq!(off.audio, sp_core::prod_env::audio::AudioProfile::default());
}

// ---- what a check logs ----

#[test]
fn each_change_is_logged_once() {
    let edition = |e: &str| not_allowed(e, Some(22631));
    let mut r = Reporter::default();
    assert_eq!(r.step(&[], &[]), LogStep::Holds);
    assert_eq!(r.step(&[], &[]), LogStep::Quiet);
    assert_eq!(
        r.step(&[EnvProblem::DefenderRealtimeOn], &[]),
        LogStep::Problems
    );
    assert_eq!(
        r.step(&[EnvProblem::DefenderRealtimeOn], &[]),
        LogStep::Quiet
    );
    let two = [
        EnvProblem::DefenderRealtimeOn,
        EnvProblem::FirewallOn(FirewallProfile::Public),
    ];
    assert_eq!(r.step(&two, &[]), LogStep::Problems);
    assert_eq!(r.step(&two, &[]), LogStep::Quiet);
    assert_eq!(r.step(&[edition("Core")], &[]), LogStep::Problems);
    // The same code, another edition: a change.
    assert_eq!(r.step(&[edition("Professional")], &[]), LogStep::Problems);
    assert_eq!(r.step(&[], &[]), LogStep::Cleared);
    assert_eq!(r.step(&[], &[]), LogStep::Quiet);
}

#[test]
fn a_first_check_with_problems_logs_them() {
    let mut r = Reporter::default();
    assert_eq!(
        r.step(&[EnvProblem::DefenderRealtimeOn], &[]),
        LogStep::Problems
    );
    assert_eq!(r.step(&[], &[]), LogStep::Cleared);
}

/// Review finding 8: the same problems with another reason a fact could not
/// be read is a change too (one WARN), and that reason staying is quiet.
#[test]
fn a_changed_read_error_is_warned_once() {
    let unread = [EnvProblem::Unreadable(Unreadable::Defender)];
    let timeout = ["the WMI check did not finish within 30s".to_string()];
    let refused = ["Defender's status could not be read (0x80070005)".to_string()];
    let mut r = Reporter::default();
    assert_eq!(r.step(&unread, &timeout), LogStep::Problems);
    assert_eq!(r.step(&unread, &timeout), LogStep::Quiet);
    assert_eq!(r.step(&unread, &refused), LogStep::ReadErrors);
    assert_eq!(r.step(&unread, &refused), LogStep::Quiet);
    // A problem change wins over a reason change.
    assert_eq!(r.step(&[], &timeout), LogStep::Cleared);
    // No problem: a reason that goes (or comes) is still a change.
    assert_eq!(r.step(&[], &[]), LogStep::ReadErrors);
    assert_eq!(r.step(&[], &timeout), LogStep::ReadErrors);
    assert_eq!(r.step(&[], &timeout), LogStep::Quiet);
}

#[test]
fn a_first_check_logs_its_problems_whatever_its_read_errors() {
    let why = ["ProductName is missing".to_string()];
    assert_eq!(Reporter::default().step(&[], &why), LogStep::Holds);
    assert_eq!(
        Reporter::default().step(&[EnvProblem::DefenderRealtimeOn], &why),
        LogStep::Problems
    );
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
