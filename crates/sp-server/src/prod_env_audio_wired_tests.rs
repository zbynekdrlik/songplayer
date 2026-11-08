//! #285: every wired adapter's values, on the walk's test registry.

use sp_core::prod_env::{EnvProblem, Unreadable};

use super::super::fake::{Fake, class_key, connection, t};
use super::super::{IfRow, NET_CLASS, RegValue};
use super::*;

const GUID: &str = "{AAAAAAAA-0000-4000-8000-000000000001}";

fn wired_row(guid: &str) -> IfRow {
    IfRow {
        guid: guid.to_string(),
        mac: "6c1ff773c7d9".to_string(),
        permanent_mac: "6c1ff773c7d9".to_string(),
        hardware: true,
        filter: false,
        medium: MEDIUM_802_3,
        present: true,
        enabled: true,
    }
}

/// resolume-snv's Dante NIC after the script: the eight always-values as
/// REG_SZ "0", `PnPCapabilities` 24.
fn dante_box() -> Fake {
    let mut reg = Fake::default();
    reg.add_instance("0014");
    let key = class_key("0014");
    reg.set(&key, "NetCfgInstanceId", t(GUID));
    reg.set(&key, "DriverDesc", t("Realtek USB GbE Family Controller"));
    for keyword in ALWAYS {
        reg.set(&key, keyword, t("0"));
    }
    reg.set(&key, PNP_CAPABILITIES, RegValue::Dword(24));
    reg.set(&connection(GUID), "Name", t("Dante"));
    reg
}

fn read(reg: &Fake) -> Option<Vec<WiredNicFacts>> {
    wired_nics(reg, &Ok(vec![wired_row(GUID)]), &mut Vec::new())
}

fn value_problem(keyword: &str, value: Option<&str>) -> EnvProblem {
    EnvProblem::WiredNicValue {
        nic: "Dante".to_string(),
        keyword: keyword.to_string(),
        value: value.map(str::to_string),
    }
}

#[test]
fn a_configured_usb_adapter_holds_with_the_optional_values_absent() {
    let nics = read(&dante_box());
    assert_eq!(problems(&nics), []);
    let view = view(&nics.unwrap()[0]);
    assert_eq!(view.ok, Some(true));
    assert_eq!(view.name.as_deref(), Some("Dante"));
    assert_eq!(view.values.len(), 13, "8 + 4 + PnPCapabilities");
}

#[test]
fn a_driver_install_that_deleted_the_values_names_each_of_them() {
    let mut reg = Fake::default();
    reg.add_instance("0014");
    reg.set(&class_key("0014"), "NetCfgInstanceId", t(GUID));
    reg.set(&connection(GUID), "Name", t("Dante"));
    let mut wanted: Vec<EnvProblem> = ALWAYS.iter().map(|k| value_problem(k, None)).collect();
    wanted.push(value_problem(PNP_CAPABILITIES, None));
    let nics = read(&reg);
    assert_eq!(problems(&nics), wanted);
    assert_eq!(view(&nics.unwrap()[0]).ok, Some(false));
}

#[test]
fn a_value_on_is_named_with_what_was_read() {
    let mut reg = dante_box();
    reg.set(&class_key("0014"), "*EEE", t("1"));
    reg.set(&class_key("0014"), "*FlowControl", RegValue::Dword(3));
    reg.set(&class_key("0014"), "GigaLite", t("off"));
    assert_eq!(
        problems(&read(&reg)),
        [
            value_problem("*FlowControl", Some("3")),
            value_problem("*EEE", Some("1")),
            value_problem("GigaLite", Some("off")),
        ]
    );
}

#[test]
fn an_optional_value_counts_only_where_the_adapter_has_it() {
    let mut reg = dante_box();
    reg.set(&class_key("0014"), "*InterruptModeration", t("1"));
    assert_eq!(
        problems(&read(&reg)),
        [value_problem("*InterruptModeration", Some("1"))]
    );
    reg.set(&class_key("0014"), "*InterruptModeration", t("0"));
    assert_eq!(problems(&read(&reg)), []);
}

#[test]
fn pnp_capabilities_needs_the_disable_pm_bit_alone() {
    let mut reg = dante_box();
    for (value, holds) in [(24, true), (8, true), (280, true), (16, false), (0, false)] {
        reg.set(&class_key("0014"), PNP_CAPABILITIES, RegValue::Dword(value));
        let wanted = if holds {
            vec![]
        } else {
            vec![value_problem(PNP_CAPABILITIES, Some(&value.to_string()))]
        };
        assert_eq!(problems(&read(&reg)), wanted, "PnPCapabilities {value}");
    }
}

#[test]
fn a_value_that_cannot_be_read_is_unreadable_never_fine() {
    let mut reg = dante_box();
    reg.set(&class_key("0014"), "*EEE", RegValue::Failed(5));
    let mut errors = Vec::new();
    let nics = wired_nics(&reg, &Ok(vec![wired_row(GUID)]), &mut errors);
    assert_eq!(
        problems(&nics),
        [EnvProblem::WiredNicUnreadable {
            nic: "Dante".to_string()
        }]
    );
    assert_eq!(view(&nics.unwrap()[0]).ok, None);
    assert!(errors[0].contains("*EEE"), "{errors:?}");
    assert!(errors[0].contains("error 5"), "{errors:?}");
}

#[test]
fn an_adapter_not_in_the_network_class_is_unreadable() {
    let mut reg = Fake::default();
    reg.subkeys_of(NET_CLASS, Ok(vec!["Properties".to_string()]));
    reg.set(&connection(GUID), "Name", t("Dante"));
    let mut errors = Vec::new();
    let nics = wired_nics(&reg, &Ok(vec![wired_row(GUID)]), &mut errors);
    assert_eq!(
        problems(&nics),
        [EnvProblem::WiredNicUnreadable {
            nic: "Dante".to_string()
        }]
    );
    assert_eq!(view(&nics.unwrap()[0]).ok, None);
    assert_eq!(errors.len(), 1, "{errors:?}");
}

#[test]
fn interfaces_that_cannot_be_listed_are_unreadable() {
    let mut errors = Vec::new();
    let nics = wired_nics(&dante_box(), &Err(87), &mut errors);
    assert_eq!(nics, None);
    assert_eq!(
        problems(&nics),
        [EnvProblem::Unreadable(Unreadable::WiredNics)]
    );
    assert!(errors[0].contains("error 87"), "{errors:?}");
}

#[test]
fn only_a_hardware_802_3_row_of_its_own_is_an_adapter() {
    assert!(is_wired(&wired_row(GUID)));
    let filter = IfRow {
        filter: true,
        ..wired_row(GUID)
    };
    let software = IfRow {
        hardware: false,
        ..wired_row(GUID)
    };
    // resolume-snv's Wi-Fi is `Native 802.11`, medium 9.
    let radio = IfRow {
        medium: 9,
        ..wired_row(GUID)
    };
    assert!(!is_wired(&filter));
    assert!(!is_wired(&software));
    assert!(!is_wired(&radio));
    let nics = wired_nics(
        &dante_box(),
        &Ok(vec![filter, software, radio]),
        &mut Vec::new(),
    );
    assert_eq!(nics, Some(vec![]));
}

/// resolume-snv 8.11.2026: "Ethernet 2" and "Ethernet 3" are rows of removed
/// adapters (`OperStatus` 6, no device, no class key). They carry no traffic
/// and `setup-prod-pc` never sees them, so they are no adapter to gate.
#[test]
fn a_removed_adapters_row_is_no_adapter() {
    let ghost = IfRow {
        present: false,
        ..wired_row("{F6686CC8-B055-43F2-A33A-0244DB4140FA}")
    };
    assert!(!is_wired(&ghost));
    let mut errors = Vec::new();
    let nics = wired_nics(&dante_box(), &Ok(vec![wired_row(GUID), ghost]), &mut errors).unwrap();
    let guids: Vec<&str> = nics.iter().map(|n| n.guid.as_str()).collect();
    assert_eq!(guids, [GUID]);
    assert_eq!(problems(&Some(nics)), []);
    assert_eq!(errors, Vec::<String>::new());
}

/// A disabled adapter keeps its row (resolume-snv's disabled Wi-Fi: OperStatus
/// 2, AdminStatus 2). `setup-prod-pc` leaves a disabled adapter alone, so
/// the gate does too.
#[test]
fn a_disabled_adapter_is_no_adapter_to_gate() {
    let off = IfRow {
        enabled: false,
        ..wired_row("{BBBBBBBB-0000-4000-8000-000000000002}")
    };
    assert!(!is_wired(&off));
    let nics = wired_nics(
        &dante_box(),
        &Ok(vec![wired_row(GUID), off]),
        &mut Vec::new(),
    )
    .unwrap();
    let guids: Vec<&str> = nics.iter().map(|n| n.guid.as_str()).collect();
    assert_eq!(guids, [GUID]);
}

#[test]
fn adapters_come_in_a_stable_order() {
    let b = "{BBBBBBBB-0000-4000-8000-000000000002}";
    let mut reg = dante_box();
    reg.add_instance("0015");
    reg.set(&class_key("0015"), "NetCfgInstanceId", t(b));
    let rows = Ok(vec![wired_row(b), wired_row(GUID), wired_row(GUID)]);
    let nics = wired_nics(&reg, &rows, &mut Vec::new()).unwrap();
    let guids: Vec<&str> = nics.iter().map(|n| n.guid.as_str()).collect();
    assert_eq!(guids, [GUID, b]);
}

/// One check's facts carry the adapters, and their problems gate the box.
#[test]
fn a_check_reads_every_wired_adapter_into_the_gate() {
    let mut reg = dante_box();
    reg.set(&class_key("0014"), "*EEE", t("1"));
    let facts = super::super::read_audio(
        &reg,
        &Ok(vec![wired_row(GUID)]),
        &super::super::FileRead::Missing,
        &super::super::FileRead::Missing,
        &super::super::fake::power_read(),
    );
    assert_eq!(facts.wired_nics.as_ref().map(Vec::len), Some(1));
    assert!(
        crate::prod_env::audio::problems(&facts).contains(&value_problem("*EEE", Some("1"))),
        "{:?}",
        crate::prod_env::audio::problems(&facts)
    );
    let shown = crate::prod_env::audio::view(&facts).wired_nics.unwrap();
    assert_eq!(shown[0].ok, Some(false));
    assert_eq!(shown[0].name.as_deref(), Some("Dante"));
}

/// The script writes what this check reads: the same two lists.
#[test]
fn the_lists_are_the_setup_scripts() {
    let script = include_str!("../../../scripts/setup-prod-pc.ps1");
    let line = |names: &[&str]| {
        let quoted: Vec<String> = names.iter().map(|n| format!("'{n}'")).collect();
        format!("return @({})", quoted.join(", "))
    };
    assert!(script.contains(&line(&ALWAYS)), "{}", line(&ALWAYS));
    assert!(
        script.contains(&line(&WHERE_PRESENT)),
        "{}",
        line(&WHERE_PRESENT)
    );
}
