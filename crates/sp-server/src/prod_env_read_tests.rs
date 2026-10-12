//! #233: the raw reads made into facts — the firewall's active store and
//! the registry's fallback, Defender's WMI read, and every fact of one check
//! (`facts_from`).

use sp_core::prod_env::FirewallProfile;

use super::*;
use crate::prod_env::{EnvFacts, WBEM_E_INVALID_CLASS, WBEM_E_INVALID_NAMESPACE};

// ---- the firewall: the active store decides ----

fn row(name: &str, enabled: Option<u32>) -> FirewallRow {
    FirewallRow {
        name: name.to_string(),
        enabled,
    }
}

/// The active store's three profiles, each `Enabled` value as given.
fn store(domain: u32, private: u32, public: u32) -> Vec<FirewallRow> {
    vec![
        row("Domain", Some(domain)),
        row("Private", Some(private)),
        row("Public", Some(public)),
    ]
}

#[test]
fn each_profile_reads_its_own_row_in_the_active_store() {
    let rows = store(0, 1, 0);
    assert_eq!(active_firewall(&rows, FirewallProfile::Domain), Ok(false));
    assert_eq!(active_firewall(&rows, FirewallProfile::Private), Ok(true));
    assert_eq!(active_firewall(&rows, FirewallProfile::Public), Ok(false));
    // The provider's names, whatever their case or padding.
    let named = vec![row(" PUBLIC ", Some(1))];
    assert_eq!(active_firewall(&named, FirewallProfile::Public), Ok(true));
}

#[test]
fn an_active_store_profile_that_cannot_be_told_says_why() {
    let rows = vec![row("Domain", Some(2)), row("Private", None)];
    assert_eq!(
        active_firewall(&rows, FirewallProfile::Domain),
        Err("the domain firewall profile reads 2 in the active store".to_string())
    );
    assert_eq!(
        active_firewall(&rows, FirewallProfile::Private),
        Err("the private firewall profile has no state in the active store".to_string())
    );
    assert_eq!(
        active_firewall(&rows, FirewallProfile::Public),
        Err("the active firewall store has no public profile".to_string())
    );
}

/// Review finding 1: the older Administrative-Templates policy
/// (`WindowsFirewall\StandardProfile`) applies to the private and public
/// profiles, so a policy that turns them on while the local value is off
/// reads on (the gate passed such a box before).
#[test]
fn the_older_standard_profile_policy_turns_private_and_public_on() {
    use super::RegDword::{Absent, Failed, Value};
    for p in [FirewallProfile::Private, FirewallProfile::Public] {
        assert_eq!(
            firewall_from_registry(p, Absent, Value(1), Value(0)),
            Ok(true),
            "{p:?}"
        );
        assert_eq!(
            firewall_from_registry(p, Value(0), Value(1), Value(0)),
            Ok(true),
            "{p:?}"
        );
        assert_eq!(
            firewall_from_registry(p, Absent, Value(0), Value(1)),
            Ok(false),
            "{p:?}"
        );
        assert_eq!(
            firewall_from_registry(p, Value(1), Value(0), Value(0)),
            Ok(true),
            "{p:?}"
        );
        assert_eq!(
            firewall_from_registry(p, Absent, Failed(5), Value(0)),
            Err(format!(
                "the {} firewall older policy (StandardProfile) could not be read (error 5)",
                p.name()
            )),
            "{p:?}"
        );
    }
    // Not the domain profile's.
    let d = FirewallProfile::Domain;
    assert_eq!(
        firewall_from_registry(d, Absent, Value(1), Value(0)),
        Ok(false)
    );
    assert_eq!(
        firewall_from_registry(d, Absent, Failed(5), Value(0)),
        Ok(false)
    );
}

// ---- the firewall's registry fallback: the policy wins over the local value ----

#[test]
fn a_firewall_policy_wins_over_the_local_value() {
    use super::RegDword::{Absent, Failed, Value};
    let d = FirewallProfile::Domain;
    assert_eq!(
        firewall_from_registry(d, Value(0), Absent, Value(1)),
        Ok(false)
    );
    assert_eq!(
        firewall_from_registry(d, Value(1), Absent, Value(0)),
        Ok(true)
    );
    assert_eq!(
        firewall_from_registry(d, Value(2), Absent, Absent),
        Ok(true)
    );
    assert_eq!(
        firewall_from_registry(d, Value(0), Absent, Failed(5)),
        Ok(false)
    );
}

#[test]
fn with_no_policy_the_local_value_decides() {
    use super::RegDword::{Absent, Value};
    let p = FirewallProfile::Private;
    assert_eq!(
        firewall_from_registry(p, Absent, Absent, Value(0)),
        Ok(false)
    );
    assert_eq!(
        firewall_from_registry(p, Absent, Absent, Value(1)),
        Ok(true)
    );
    assert_eq!(
        firewall_from_registry(p, Absent, Absent, Value(7)),
        Ok(true)
    );
}

#[test]
fn a_firewall_that_cannot_be_read_says_why() {
    use super::RegDword::{Absent, Failed, Value};
    assert_eq!(
        firewall_from_registry(FirewallProfile::Domain, Failed(5), Absent, Value(0)),
        Err("the domain firewall policy could not be read (error 5)".to_string())
    );
    assert_eq!(
        firewall_from_registry(FirewallProfile::Private, Absent, Absent, Absent),
        Err("the private firewall setting is missing".to_string())
    );
    assert_eq!(
        firewall_from_registry(FirewallProfile::Public, Absent, Absent, Failed(1630)),
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

// ---- one check's facts ----

/// A production box's raw reads: IoT Enterprise LTSC, Defender real-time
/// off, every firewall profile off (locally, no policy).
fn direct() -> DirectRead {
    DirectRead {
        edition_id: Ok(Some("IoTEnterpriseS".to_string())),
        product_name: Ok(Some("Windows 10 IoT Enterprise LTSC 2024".to_string())),
        build: Ok(Some("26100".to_string())),
        firewall_policy: [RegDword::Absent; 3],
        firewall_legacy_policy: RegDword::Absent,
        firewall_local: [RegDword::Value(0); 3],
        defender_service: ServiceRead::State(4),
    }
}

fn wmi() -> WmiRead {
    WmiRead {
        defender: Ok(Some(DefenderRow {
            realtime: Some(false),
            mode: Some("Normal".to_string()),
            antivirus_enabled: Some(true),
        })),
        firewall: Ok(store(0, 0, 0)),
        antivirus: Ok(Vec::new()),
    }
}

#[test]
fn a_production_box_reads_as_its_facts() {
    assert_eq!(
        facts_from(&direct(), &Ok(wmi())),
        EnvFacts {
            edition_id: Some("IoTEnterpriseS".to_string()),
            product_name: Some("Windows 10 IoT Enterprise LTSC 2024".to_string()),
            build_number: Some(26100),
            defender_realtime: Some(false),
            firewall: [Some(false); 3],
            read_errors: Vec::new(),
        }
    );
}

#[test]
fn each_registry_fact_says_why_it_is_missing() {
    let read = DirectRead {
        edition_id: Ok(None),
        product_name: Err(5),
        ..direct()
    };
    let facts = facts_from(&read, &Ok(wmi()));
    assert_eq!(facts.edition_id, None);
    assert_eq!(facts.product_name, None);
    assert_eq!(
        facts.read_errors,
        [
            "EditionID is missing",
            "ProductName could not be read (error 5)",
        ]
    );
}

/// Review finding 1: the active store decides each profile, whatever the
/// registry says (the policy that turned a profile on lives there).
#[test]
fn the_active_store_decides_the_firewall() {
    let on = WmiRead {
        firewall: Ok(store(1, 0, 1)),
        ..wmi()
    };
    // The registry says every profile is off.
    let facts = facts_from(&direct(), &Ok(on));
    assert_eq!(facts.firewall, [Some(true), Some(false), Some(true)]);
    assert!(facts.read_errors.is_empty(), "{:?}", facts.read_errors);
}

/// The active store not read: the registry decides, and says so; a profile
/// neither can tell is unread, with both reasons.
#[test]
fn a_failed_active_store_leaves_the_registry_to_decide_and_says_so() {
    let refused = WmiRead {
        firewall: Err(0x8007_0005_u32 as i32),
        ..wmi()
    };
    let read = DirectRead {
        firewall_local: [RegDword::Value(1), RegDword::Absent, RegDword::Value(0)],
        ..direct()
    };
    let facts = facts_from(&read, &Ok(refused));
    assert_eq!(facts.firewall, [Some(true), None, Some(false)]);
    assert_eq!(
        facts.read_errors,
        [
            "the active firewall store could not be read (0x80070005): the registry decided",
            "the active firewall store could not be read (0x80070005)",
            "the private firewall setting is missing",
        ]
    );
    // A store with no row for a profile: that profile alone falls back.
    let partial = WmiRead {
        firewall: Ok(vec![row("Domain", Some(0)), row("Public", Some(1))]),
        ..wmi()
    };
    let facts = facts_from(&direct(), &Ok(partial));
    assert_eq!(facts.firewall, [Some(false), Some(false), Some(true)]);
    assert_eq!(
        facts.read_errors,
        ["the active firewall store has no private profile: the registry decided"]
    );
}

#[test]
fn the_build_number_is_read_or_says_why() {
    for (read, build, why) in [
        (Ok(Some(" 22631 ".to_string())), Some(22631), None),
        (Ok(None), None, Some("CurrentBuildNumber is missing")),
        (
            Err(2),
            None,
            Some("CurrentBuildNumber could not be read (error 2)"),
        ),
        (
            Ok(Some("22H2".to_string())),
            None,
            Some("CurrentBuildNumber reads \"22H2\""),
        ),
    ] {
        let facts = facts_from(
            &DirectRead {
                build: read.clone(),
                ..direct()
            },
            &Ok(wmi()),
        );
        assert_eq!(facts.build_number, build, "{read:?}");
        let whys: Vec<&str> = why.into_iter().collect();
        assert_eq!(facts.read_errors, whys, "{read:?}");
    }
}

#[test]
fn a_wmi_read_that_did_not_finish_leaves_the_registry_facts() {
    let why = "the WMI check did not finish within 30s".to_string();
    let facts = facts_from(&direct(), &Err(why.clone()));
    assert_eq!(facts.edition_id, Some("IoTEnterpriseS".to_string()));
    // The registry decides the firewall while the active store is not read.
    assert_eq!(facts.firewall, [Some(false); 3]);
    assert_eq!(facts.defender_realtime, None);
    assert_eq!(
        facts.read_errors,
        [format!("{why}: the registry decided"), why]
    );
}

#[test]
fn defenders_query_decides_its_fact() {
    let on = WmiRead {
        defender: Ok(Some(DefenderRow {
            realtime: Some(true),
            ..DefenderRow::default()
        })),
        ..wmi()
    };
    assert_eq!(facts_from(&direct(), &Ok(on)).defender_realtime, Some(true));
    let refused = WmiRead {
        defender: Err(0x8007_0005_u32 as i32),
        ..wmi()
    };
    let facts = facts_from(&direct(), &Ok(refused));
    assert_eq!(facts.defender_realtime, None);
    assert_eq!(
        facts.read_errors,
        ["Defender's status could not be read (0x80070005)"]
    );
}

#[test]
fn one_reason_is_noted_once() {
    let mut errors = Vec::new();
    note(&mut errors, "a".to_string());
    note(&mut errors, "b".to_string());
    note(&mut errors, "a".to_string());
    assert_eq!(errors, ["a", "b"]);
}

#[test]
fn every_wmi_query_fails_with_one_code() {
    assert_eq!(
        WmiRead::failed(-5),
        WmiRead {
            defender: Err(-5),
            firewall: Err(-5),
            antivirus: Err(-5),
        }
    );
}
