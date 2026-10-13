//! #233: the production environment's codes, Slovak texts, status wire shape
//! and health segment.

use super::*;
use crate::health::HealthTone;

/// Every problem, one of each kind and profile.
fn every_problem() -> Vec<EnvProblem> {
    let mut all = vec![
        EnvProblem::EditionNotAllowed {
            edition: "Professional".to_string(),
            build: Some(22631),
        },
        EnvProblem::EditionNotAllowed {
            edition: "IoTEnterprise".to_string(),
            build: None,
        },
        EnvProblem::DefenderRealtimeOn,
        EnvProblem::DefenderPassive("EDR Block Mode".to_string()),
        EnvProblem::AntivirusOn("ESET Security".to_string()),
    ];
    for p in FirewallProfile::ALL {
        all.push(EnvProblem::FirewallOn(p));
    }
    all.push(EnvProblem::Unreadable(Unreadable::Edition));
    all.push(EnvProblem::Unreadable(Unreadable::Build));
    all.push(EnvProblem::Unreadable(Unreadable::Defender));
    all.push(EnvProblem::Unreadable(Unreadable::Antivirus));
    for p in FirewallProfile::ALL {
        all.push(EnvProblem::Unreadable(Unreadable::Firewall(p)));
    }
    all
}

#[test]
fn every_problem_has_its_stable_code() {
    let codes: Vec<String> = every_problem().iter().map(EnvProblem::code).collect();
    assert_eq!(
        codes,
        [
            "edition_not_allowed",
            "edition_not_allowed",
            "defender_realtime_on",
            "defender_passive_mode",
            "antivirus_on",
            "firewall_on:domain",
            "firewall_on:private",
            "firewall_on:public",
            "unreadable:edition",
            "unreadable:build",
            "unreadable:defender",
            "unreadable:antivirus",
            "unreadable:firewall:domain",
            "unreadable:firewall:private",
            "unreadable:firewall:public",
        ]
    );
}

#[test]
fn every_problem_reads_in_slovak() {
    let texts: Vec<String> = every_problem().iter().map(EnvProblem::text_sk).collect();
    assert_eq!(
        texts,
        [
            "edícia Windows „Professional“ (zostava 22631) nie je povolená (povolené sú len \
             Windows LTSC a Windows 11 IoT Enterprise)",
            "edícia Windows „IoTEnterprise“ nie je povolená (povolené sú len Windows LTSC a \
             Windows 11 IoT Enterprise)",
            "ochrana Windows Defender v reálnom čase je zapnutá",
            "Windows Defender beží v režime „EDR Block Mode“ a súbory stále kontroluje",
            "antivírus „ESET Security“ je zapnutý",
            "brána firewall je zapnutá (doménový profil)",
            "brána firewall je zapnutá (súkromný profil)",
            "brána firewall je zapnutá (verejný profil)",
            "edíciu Windows sa nepodarilo zistiť",
            "zostavu Windows sa nepodarilo zistiť",
            "stav ochrany Windows Defender v reálnom čase sa nepodarilo zistiť",
            "zoznam antivírusových programov sa nepodarilo zistiť",
            "stav brány firewall (doménový profil) sa nepodarilo zistiť",
            "stav brány firewall (súkromný profil) sa nepodarilo zistiť",
            "stav brány firewall (verejný profil) sa nepodarilo zistiť",
        ]
    );
}

#[test]
fn a_problem_is_shown_by_its_code_and_text() {
    let p = EnvProblem::FirewallOn(FirewallProfile::Public);
    assert_eq!(
        p.view(),
        ProblemView {
            code: "firewall_on:public".to_string(),
            text_sk: "brána firewall je zapnutá (verejný profil)".to_string(),
        }
    );
}

#[test]
fn the_profiles_are_listed_domain_private_public() {
    let names: Vec<&str> = FirewallProfile::ALL.iter().map(|p| p.name()).collect();
    assert_eq!(names, ["domain", "private", "public"]);
}

/// The wire shape both routes answer, every field by name.
#[test]
fn the_status_serializes_every_field_by_name() {
    let status = EnvironmentStatus {
        checked: true,
        ok: false,
        note: None,
        edition_id: Some("IoTEnterpriseS".to_string()),
        product_name: Some("Windows 10 IoT Enterprise LTSC".to_string()),
        build_number: Some(26100),
        ltsc: Some(true),
        edition_allowed: Some(true),
        defender_realtime: Some(true),
        defender_passive: Some(false),
        defender_mode: Some("Normal".to_string()),
        defender_antivirus_enabled: Some(true),
        antivirus_on: Some(vec!["ESET Security".to_string()]),
        firewall: FirewallState {
            domain: Some(false),
            private: None,
            public: Some(true),
        },
        problems: vec![EnvProblem::DefenderRealtimeOn.view()],
        read_errors: vec!["firewall private: error 5".to_string()],
        checked_at_ms: Some(1_791_608_681_366),
    };
    let json = serde_json::to_value(&status).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "checked": true,
            "ok": false,
            "note": null,
            "edition_id": "IoTEnterpriseS",
            "product_name": "Windows 10 IoT Enterprise LTSC",
            "build_number": 26100,
            "ltsc": true,
            "edition_allowed": true,
            "defender_realtime": true,
            "defender_passive": false,
            "defender_mode": "Normal",
            "defender_antivirus_enabled": true,
            "antivirus_on": ["ESET Security"],
            "firewall": {"domain": false, "private": null, "public": true},
            "problems": [{
                "code": "defender_realtime_on",
                "text_sk": "ochrana Windows Defender v reálnom čase je zapnutá",
            }],
            "read_errors": ["firewall private: error 5"],
            "checked_at_ms": 1_791_608_681_366_i64,
        })
    );
    let back: EnvironmentStatus = serde_json::from_value(json).unwrap();
    assert_eq!(back, status);
}

/// An older server (or the mock) without the field reads as not checked.
#[test]
fn a_missing_status_reads_as_not_checked() {
    let s: EnvironmentStatus = serde_json::from_str("{}").unwrap();
    assert_eq!(s, EnvironmentStatus::default());
    assert!(!s.checked);
    assert!(!s.ok);
    assert!(s.problems.is_empty());
}

fn status_with(problems: &[EnvProblem]) -> EnvironmentStatus {
    EnvironmentStatus {
        checked: true,
        ok: problems.is_empty(),
        problems: problems.iter().map(EnvProblem::view).collect(),
        ..EnvironmentStatus::default()
    }
}

#[test]
fn the_marker_says_unchecked_ok_or_violated() {
    assert_eq!(marker(&EnvironmentStatus::default()), "unchecked");
    assert_eq!(marker(&status_with(&[])), "ok");
    assert_eq!(
        marker(&status_with(&[EnvProblem::DefenderRealtimeOn])),
        "violated"
    );
    // Not ok with no problem named, or ok beside a problem: never "ok".
    let not_ok = EnvironmentStatus {
        checked: true,
        ok: false,
        ..EnvironmentStatus::default()
    };
    assert_eq!(marker(&not_ok), "violated");
    let ok_with_a_problem = EnvironmentStatus {
        ok: true,
        ..status_with(&[EnvProblem::DefenderRealtimeOn])
    };
    assert_eq!(marker(&ok_with_a_problem), "violated");
    // Not checked wins over anything else.
    let unchecked = EnvironmentStatus {
        checked: false,
        ..status_with(&[EnvProblem::DefenderRealtimeOn])
    };
    assert_eq!(marker(&unchecked), "unchecked");
}

#[test]
fn no_segment_before_the_first_check_or_while_the_box_holds() {
    assert_eq!(label(&EnvironmentStatus::default()), None);
    assert_eq!(label(&status_with(&[])), None);
}

#[test]
fn one_problem_is_the_red_segment_and_the_tooltip_names_the_rule() {
    let (tone, text, tip) = label(&status_with(&[EnvProblem::DefenderRealtimeOn])).unwrap();
    assert_eq!(tone, HealthTone::Error);
    assert_eq!(
        text,
        "Prostredie: ochrana Windows Defender v reálnom čase je zapnutá"
    );
    assert_eq!(
        tip,
        format!("{RULE_SK} Zistené: ochrana Windows Defender v reálnom čase je zapnutá.")
    );
}

#[test]
fn more_problems_show_the_first_and_count_the_rest() {
    let two = [
        EnvProblem::DefenderRealtimeOn,
        EnvProblem::FirewallOn(FirewallProfile::Domain),
    ];
    let (_, text, tip) = label(&status_with(&two)).unwrap();
    assert_eq!(
        text,
        "Prostredie: ochrana Windows Defender v reálnom čase je zapnutá (+1)"
    );
    assert_eq!(
        tip,
        format!(
            "{RULE_SK} Zistené: ochrana Windows Defender v reálnom čase je zapnutá; brána \
             firewall je zapnutá (doménový profil)."
        )
    );
    let three = [
        EnvProblem::Unreadable(Unreadable::Edition),
        EnvProblem::DefenderRealtimeOn,
        EnvProblem::FirewallOn(FirewallProfile::Public),
    ];
    let (_, text, _) = label(&status_with(&three)).unwrap();
    assert_eq!(text, "Prostredie: edíciu Windows sa nepodarilo zistiť (+2)");
}
