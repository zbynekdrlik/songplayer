//! #285: every wired adapter's values, on the walk's test registry.

use sp_core::prod_env::{EnvProblem, Unreadable};

use super::super::fake::{Fake, class_key, connection, t};
use super::super::{IfRow, NET_CLASS, RegValue};
use super::*;

const GUID: &str = "{AAAAAAAA-0000-4000-8000-000000000001}";

/// A Realtek RTL8153 (USB GbE): no RTL8156, so none of its values.
const RTL8153: &str = r"USB\VID_0BDA&PID_8153\000001";

/// resolume-snv's `Ethernet 4` (PnPDeviceID as read on 10.11.2026).
const RTL8156: &str = r"USB\VID_0BDA&PID_8156\4F13005C8C";

fn wired_row(guid: &str) -> IfRow {
    IfRow {
        guid: guid.to_string(),
        mac: "6c1ff773c7d9".to_string(),
        permanent_mac: "6c1ff773c7d9".to_string(),
        hardware: true,
        filter: false,
        medium: MEDIUM_802_3,
        if_type: IF_TYPE_ETHERNET,
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
    reg.set(&key, "DeviceInstanceID", t(RTL8153));
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
    reg.set(&class_key("0014"), "DeviceInstanceID", t(RTL8153));
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
        if_type: 71,
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

/// The review of 8.11.2026: a driver that reports no physical medium (0,
/// unspecified) on an Ethernet interface is still a wired adapter, as the
/// script sees it (`Get-NetAdapter` `MediaType` 802.3 is the NDIS medium).
#[test]
fn an_ethernet_row_with_no_physical_medium_is_an_adapter() {
    let unspecified = IfRow {
        medium: 0,
        ..wired_row(GUID)
    };
    assert!(is_wired(&unspecified));
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
        &super::super::fake::power_read(),
    );
    assert_eq!(facts.wired_nics.as_ref().map(Vec::len), Some(1));
    assert!(
        crate::prod_env::audio::problems(&facts, &[]).contains(&value_problem("*EEE", Some("1"))),
        "{:?}",
        crate::prod_env::audio::problems(&facts, &[])
    );
    let shown = crate::prod_env::audio::view(&facts, &[])
        .wired_nics
        .unwrap();
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

/// resolume-snv's `Ethernet 4` before dstack's A/B (10.11.2026): the
/// configured values of every adapter, and "Transmit URBs" 3 / "Transmit
/// Buffers" 18 as its driver had them.
fn rtl8156_box() -> Fake {
    let mut reg = dante_box();
    let key = class_key("0014");
    reg.set(
        &key,
        "DriverDesc",
        t("Realtek Gaming USB 2.5GbE Family Controller"),
    );
    reg.set(&key, "DeviceInstanceID", t(RTL8156));
    reg.set(&key, "PendingTransmits", t("3"));
    reg.set(&key, "TransmitBufferLen", t("18"));
    reg
}

#[test]
fn an_rtl8156_is_known_by_its_hardware_id() {
    for (id, is) in [
        (RTL8156, true),
        (r"usb\vid_0bda&pid_8156\4f13005c8c", true),
        (r" USB\VID_0BDA&PID_8156 ", true),
        (r"USB\VID_0BDA&PID_8156&MI_00\7&1", true),
        (r"USB\VID_0BDA&PID_81560\1", false),
        (RTL8153, false),
        (r"PCI\VEN_10EC&DEV_8125&SUBSYS_0", false),
        (r"XUSB\VID_0BDA&PID_8156\1", false),
        ("", false),
    ] {
        assert_eq!(is_rtl8156(id), is, "{id:?}");
    }
}

#[test]
fn the_rtl8156s_values_are_dstacks_and_every_other_value_is_0() {
    assert_eq!(wanted("PendingTransmits"), 8);
    assert_eq!(wanted("TransmitBufferLen"), 64);
    assert_eq!(wanted("*EEE"), 0);
    assert_eq!(wanted("PendingReceives"), 0);
}

#[test]
fn an_rtl8156_at_its_drivers_values_names_both() {
    let nics = read(&rtl8156_box());
    assert_eq!(
        problems(&nics),
        [
            value_problem("PendingTransmits", Some("3")),
            value_problem("TransmitBufferLen", Some("18")),
        ]
    );
    let shown = view(&nics.unwrap()[0]);
    assert_eq!(shown.ok, Some(false));
    assert_eq!(shown.values.len(), 15, "13 + the RTL8156's 2");
    assert_eq!(shown.values[13].keyword, "PendingTransmits");
    assert_eq!(shown.values[13].ok, Some(false));
    assert_eq!(shown.values[14].keyword, "TransmitBufferLen");
}

#[test]
fn an_rtl8156_at_8_and_64_holds() {
    let mut reg = rtl8156_box();
    reg.set(&class_key("0014"), "PendingTransmits", t("8"));
    reg.set(&class_key("0014"), "TransmitBufferLen", RegValue::Dword(64));
    let nics = read(&reg);
    assert_eq!(problems(&nics), []);
    let shown = view(&nics.unwrap()[0]);
    assert_eq!(shown.ok, Some(true));
    assert_eq!(shown.values[13].value.as_deref(), Some("8"));
    assert_eq!(shown.values[14].value.as_deref(), Some("64"));
}

/// Exactly the value: more is not the measured setting either, and 0 (every
/// other value's production one) is wrong here.
#[test]
fn an_rtl8156_value_is_exact() {
    let mut reg = rtl8156_box();
    reg.set(&class_key("0014"), "PendingTransmits", t("0"));
    reg.set(&class_key("0014"), "TransmitBufferLen", t("128"));
    assert_eq!(
        problems(&read(&reg)),
        [
            value_problem("PendingTransmits", Some("0")),
            value_problem("TransmitBufferLen", Some("128")),
        ]
    );
}

/// The driver offers both, so one that is gone is a problem (the deploy's
/// setup sets it through the driver).
#[test]
fn an_rtl8156_value_that_is_missing_is_named() {
    let mut reg = rtl8156_box();
    reg.set(&class_key("0014"), "PendingTransmits", RegValue::Absent);
    reg.set(&class_key("0014"), "TransmitBufferLen", t("64"));
    assert_eq!(
        problems(&read(&reg)),
        [value_problem("PendingTransmits", None)]
    );
}

#[test]
fn another_adapter_never_carries_the_rtl8156s_values() {
    let mut reg = dante_box();
    reg.set(&class_key("0014"), "PendingTransmits", t("3"));
    let nics = read(&reg);
    assert_eq!(problems(&nics), []);
    assert_eq!(view(&nics.unwrap()[0]).values.len(), 13);
}

/// PP's Dante adapter names its device in the connection only.
#[test]
fn the_connection_names_the_device_when_the_class_does_not() {
    let mut reg = rtl8156_box();
    reg.set(&class_key("0014"), "DeviceInstanceID", RegValue::Absent);
    reg.set(&connection(GUID), "PnPInstanceId", t(RTL8156));
    let nics = read(&reg);
    assert_eq!(nics.as_ref().unwrap()[0].device.as_deref(), Some(RTL8156));
    assert_eq!(problems(&nics).len(), 2, "{:?}", problems(&nics));
}

/// With no device instance it is not known whether the RTL8156's values
/// apply: unreadable, never fine.
#[test]
fn an_adapter_naming_no_device_is_unreadable() {
    let mut reg = rtl8156_box();
    reg.set(&class_key("0014"), "DeviceInstanceID", RegValue::Absent);
    reg.set(&class_key("0014"), "PendingTransmits", t("8"));
    reg.set(&class_key("0014"), "TransmitBufferLen", t("64"));
    let mut errors = Vec::new();
    let nics = wired_nics(&reg, &Ok(vec![wired_row(GUID)]), &mut errors);
    assert_eq!(
        problems(&nics),
        [EnvProblem::WiredNicUnreadable {
            nic: "Dante".to_string()
        }]
    );
    assert_eq!(view(&nics.unwrap()[0]).ok, None);
    assert_eq!(
        errors,
        [format!(
            "the network adapter {GUID} names no device instance"
        )]
    );
}

/// The script sets what this check reads: the same hardware id and pairs.
#[test]
fn the_rtl8156_rule_is_the_setup_scripts() {
    let script = include_str!("../../../scripts/setup-prod-pc.ps1");
    let id = RTL8156_DEVICE;
    assert!(script.contains(&format!("'{id}'")), "{id}");
    for (keyword, value) in RTL8156_VALUES {
        let pair = format!("[pscustomobject]@{{ Keyword = '{keyword}'; Wanted = '{value}' }}");
        assert!(script.contains(&pair), "{pair}");
    }
    // #255: only on the adapter carrying the Dante leg, only where the
    // driver offers the keyword.
    for function in [
        "function Get-ProdPcDanteCarrier",
        "function Test-ProdPcDriverOffers",
    ] {
        assert!(script.contains(function), "{function}");
    }
}

/// #255 (the owner, 11.11.2026; dstack two-networks §6, dstack PR 179): the
/// RTL8156's transmit values are judged only on the adapter carrying a
/// Dante leg. Off it (PP, no leg) they are neither a problem nor shown.
#[test]
fn an_rtl8156_off_the_dante_leg_is_not_judged() {
    let nics = read(&rtl8156_box());
    let nic = &nics.as_ref().unwrap()[0];
    for carriers in [vec![], vec!["Ethernet 4".to_string()]] {
        assert_eq!(problems_besides(&nics, &[], &carriers), []);
        let shown = view_on(nic, &carriers);
        assert_eq!(shown.ok, Some(true));
        assert_eq!(shown.values.len(), 13, "the 2 RTL8156 values left out");
    }
    let on_the_leg = [" dante ".to_string()];
    assert_eq!(
        problems_besides(&nics, &[], &on_the_leg),
        [
            value_problem("PendingTransmits", Some("3")),
            value_problem("TransmitBufferLen", Some("18")),
        ]
    );
    assert_eq!(view_on(nic, &on_the_leg).values.len(), 15);
    assert!(carries_a_leg(nic, &on_the_leg));
    assert!(!judged(nic, "PendingTransmits", &[]));
    assert!(judged(nic, "*EEE", &[]));
}

/// PP's driver 11.4.211.2022 offers neither keyword (no `Ndi\Params`
/// entry): even on a leg that passes; one it offers and the registry lacks
/// is named.
#[test]
fn a_transmit_value_the_driver_does_not_offer_passes() {
    let mut reg = rtl8156_box();
    reg.set(&class_key("0014"), "PendingTransmits", RegValue::Absent);
    reg.set(&class_key("0014"), "TransmitBufferLen", RegValue::Absent);
    let params = format!(r"{}\Ndi\Params", class_key("0014"));
    reg.subkeys_of(
        &params,
        Ok(vec!["PendingReceives".to_string(), "*EEE".to_string()]),
    );
    let nics = read(&reg);
    assert_eq!(problems(&nics), []);
    assert_eq!(view(&nics.unwrap()[0]).ok, Some(true));
    reg.subkeys_of(
        &params,
        Ok(vec![
            "PendingReceives".to_string(),
            " pendingtransmits".to_string(),
        ]),
    );
    assert_eq!(
        problems(&read(&reg)),
        [value_problem("PendingTransmits", None)]
    );
    // The list unread: taken as offered.
    reg.subkeys_of(&params, Err(5));
    assert_eq!(problems(&read(&reg)).len(), 2);
}
