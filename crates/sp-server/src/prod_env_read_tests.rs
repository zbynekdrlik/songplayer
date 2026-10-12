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
            defender_passive: Some(false),
            defender_mode: Some("Normal".to_string()),
            defender_antivirus_enabled: Some(true),
            antivirus_on: Some(Vec::new()),
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
    // Defender stays unread: the fixture's WinDefend service runs (state 4),
    // so the service manager cannot say it is off (finding 3's fallback).
    assert_eq!(
        facts.read_errors,
        [
            format!("{why}: the registry decided"),
            format!("{why}; the WinDefend service is not stopped (state 4)"),
            why,
        ]
    );
}

#[test]
fn defenders_query_decides_its_fact() {
    let on = WmiRead {
        defender: Ok(Some(DefenderRow {
            realtime: Some(true),
            mode: Some("Normal".to_string()),
            antivirus_enabled: Some(true),
        })),
        ..wmi()
    };
    assert_eq!(facts_from(&direct(), &Ok(on)).defender_realtime, Some(true));
    let refused = WmiRead {
        defender: Err(0x8007_0005_u32 as i32),
        ..wmi()
    };
    // The WinDefend service runs (state 4), so it cannot tell either.
    let facts = facts_from(&direct(), &Ok(refused));
    assert_eq!(facts.defender_realtime, None);
    assert_eq!(
        facts.read_errors,
        [
            "Defender's status could not be read (0x80070005); the WinDefend service is \
          not stopped (state 4)"
        ]
    );
}

// ---- Defender switched off entirely (review finding 3) ----

#[test]
fn the_service_says_off_when_stopped_or_not_installed() {
    assert_eq!(SERVICE_STOPPED, 1);
    assert_eq!(ERROR_SERVICE_DOES_NOT_EXIST, 1060);
    assert_eq!(defender_service_off(ServiceRead::State(1)), Ok(()));
    assert_eq!(defender_service_off(ServiceRead::Failed(1060)), Ok(()));
    for (state, why) in [
        (2, "the WinDefend service is not stopped (state 2)"),
        (4, "the WinDefend service is not stopped (state 4)"),
    ] {
        assert_eq!(
            defender_service_off(ServiceRead::State(state)),
            Err(why.to_string())
        );
    }
    assert_eq!(
        defender_service_off(ServiceRead::Failed(5)),
        Err("the WinDefend service could not be read (error 5)".to_string())
    );
    // A stopped-state number read as an error code is no stop.
    assert!(defender_service_off(ServiceRead::Failed(1)).is_err());
}

/// Review finding 3: Defender's status cannot be read (no instance, an
/// error, the WMI read timed out) while the WinDefend service is stopped
/// or not installed: Defender is off, not "cannot be verified".
#[test]
fn defender_switched_off_entirely_reads_off() {
    let stopped = DirectRead {
        defender_service: ServiceRead::State(SERVICE_STOPPED),
        ..direct()
    };
    let missing = DirectRead {
        defender_service: ServiceRead::Failed(ERROR_SERVICE_DOES_NOT_EXIST),
        ..direct()
    };
    let no_instance = WmiRead {
        defender: Ok(None),
        ..wmi()
    };
    let refused = WmiRead {
        defender: Err(0x8007_0005_u32 as i32),
        ..wmi()
    };
    let incomplete = WmiRead {
        defender: Ok(Some(DefenderRow::default())),
        ..wmi()
    };
    for read in [&stopped, &missing] {
        for status in [&no_instance, &refused, &incomplete] {
            let facts = facts_from(read, &Ok(status.clone()));
            assert_eq!(facts.defender_realtime, Some(false), "{status:?}");
            assert!(facts.read_errors.is_empty(), "{:?}", facts.read_errors);
        }
    }
    // The WMI read itself timed out: the service still tells.
    let why = "the WMI check did not finish within 30s".to_string();
    let facts = facts_from(&stopped, &Err(why));
    assert_eq!(facts.defender_realtime, Some(false));
}

#[test]
fn a_running_defender_that_cannot_be_read_stays_unread() {
    let refused = WmiRead {
        defender: Err(0x8007_0005_u32 as i32),
        ..wmi()
    };
    let unreadable_service = DirectRead {
        defender_service: ServiceRead::Failed(5),
        ..direct()
    };
    let facts = facts_from(&unreadable_service, &Ok(refused));
    assert_eq!(facts.defender_realtime, None);
    assert_eq!(
        facts.read_errors,
        [
            "Defender's status could not be read (0x80070005); the WinDefend service could \
          not be read (error 5)"
        ]
    );
}

// ---- Defender passive, other antivirus products (review finding 2) ----

#[test]
fn the_modes_that_still_scan_are_passive() {
    for mode in [
        "Passive Mode",
        "Passive",
        "EDR Block Mode",
        "SxS Passive Mode",
        " passive mode ",
    ] {
        assert!(is_passive_mode(mode), "{mode}");
    }
    for mode in ["Normal", "Not running", ""] {
        assert!(!is_passive_mode(mode), "{mode}");
    }
}

fn defender_in(mode: Option<&str>, realtime: bool) -> WmiRead {
    WmiRead {
        defender: Ok(Some(DefenderRow {
            realtime: Some(realtime),
            mode: mode.map(str::to_string),
            antivirus_enabled: Some(true),
        })),
        ..wmi()
    }
}

/// Review finding 2: in passive / EDR block mode Defender's real-time
/// protection can read off while it still scans every file.
#[test]
fn a_passive_or_edr_block_defender_still_scans() {
    for mode in ["Passive Mode", "EDR Block Mode", "SxS Passive Mode"] {
        let facts = facts_from(&direct(), &Ok(defender_in(Some(mode), false)));
        assert_eq!(facts.defender_realtime, Some(false), "{mode}");
        assert_eq!(facts.defender_passive, Some(true), "{mode}");
        assert_eq!(facts.defender_mode.as_deref(), Some(mode));
        assert_eq!(facts.defender_antivirus_enabled, Some(true));
    }
    let normal = facts_from(&direct(), &Ok(defender_in(Some("Normal"), false)));
    assert_eq!(normal.defender_passive, Some(false));
}

#[test]
fn a_defender_status_with_no_mode_cannot_be_verified() {
    let facts = facts_from(&direct(), &Ok(defender_in(None, false)));
    assert_eq!(
        (facts.defender_realtime, facts.defender_passive),
        (None, None)
    );
    assert_eq!(
        facts.read_errors,
        [
            "Defender's status names no running mode; the WinDefend service is not stopped \
          (state 4)"
        ]
    );
    // Not installed (no namespace): neither on nor passive.
    let absent = WmiRead {
        defender: Err(WBEM_E_INVALID_NAMESPACE),
        ..wmi()
    };
    let facts = facts_from(&direct(), &Ok(absent));
    assert_eq!(
        (
            facts.defender_realtime,
            facts.defender_passive,
            facts.defender_mode
        ),
        (Some(false), Some(false), None)
    );
}

#[test]
fn the_product_state_decodes_its_enabled_bit() {
    // Defender on / off, a product on and up to date, on with old
    // definitions, off, snoozed.
    for (state, on) in [
        (0x06_1100, true),
        (0x06_0100, false),
        (0x04_1000, true),
        (0x04_1010, true),
        (0x04_0000, false),
        (0x04_0010, false),
        (0x04_0100, false),
    ] {
        assert_eq!(av_product_on(state), on, "{state:#08x}");
    }
}

fn product(name: &str, guid: &str, state: Option<u32>) -> AvProduct {
    AvProduct {
        name: name.to_string(),
        guid: guid.to_string(),
        state,
    }
}

fn with_products(products: Vec<AvProduct>) -> Result<WmiRead, String> {
    Ok(WmiRead {
        antivirus: Ok(products),
        ..wmi()
    })
}

/// Review finding 2: another antivirus product that is on scans what
/// SongPlayer launches just like Defender did; it is named.
#[test]
fn an_enabled_third_party_antivirus_is_named() {
    let read = with_products(vec![
        product("Windows Defender", DEFENDER_AV_GUID, Some(0x06_1100)),
        product("ESET Security", "{1}", Some(0x04_1000)),
        product("Old Scanner", "{2}", Some(0x04_0000)),
        product("Second AV", "{3}", Some(0x04_1010)),
    ]);
    assert_eq!(
        antivirus_on(&read),
        Ok(vec!["ESET Security".to_string(), "Second AV".to_string()])
    );
    assert_eq!(
        facts_from(&direct(), &read).antivirus_on,
        Some(vec!["ESET Security".to_string(), "Second AV".to_string()])
    );
}

#[test]
fn defenders_own_entry_is_never_another_product() {
    let guid_only = product("Renamed", &DEFENDER_AV_GUID.to_lowercase(), Some(0x06_1100));
    let name_only = product(" Microsoft Defender Antivirus ", "{9}", Some(0x06_1100));
    assert!(is_defender_product(&guid_only));
    assert!(is_defender_product(&name_only));
    assert!(!is_defender_product(&product(
        "ESET Security",
        "{1}",
        Some(0)
    )));
    assert_eq!(
        antivirus_on(&with_products(vec![guid_only, name_only])),
        Ok(Vec::new())
    );
}

#[test]
fn the_antivirus_list_that_cannot_be_read_says_why() {
    let no_state = with_products(vec![product("ESET Security", "{1}", None)]);
    assert_eq!(
        antivirus_on(&no_state),
        Err("the state of the antivirus product \"ESET Security\" could not be read".to_string())
    );
    let refused = Ok(WmiRead {
        antivirus: Err(0x8007_0005_u32 as i32),
        ..wmi()
    });
    assert_eq!(
        antivirus_on(&refused),
        Err("the registered antivirus products could not be read (0x80070005)".to_string())
    );
    let facts = facts_from(&direct(), &refused);
    assert_eq!(facts.antivirus_on, None);
    assert_eq!(
        facts.read_errors,
        ["the registered antivirus products could not be read (0x80070005)"]
    );
    let why = "the WMI check did not finish within 30s".to_string();
    assert_eq!(antivirus_on(&Err(why.clone())), Err(why));
}

#[test]
fn no_security_center_lists_no_product() {
    for code in [WBEM_E_INVALID_NAMESPACE, WBEM_E_INVALID_CLASS] {
        let server = Ok(WmiRead {
            antivirus: Err(code),
            ..wmi()
        });
        assert_eq!(antivirus_on(&server), Ok(Vec::new()));
    }
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
