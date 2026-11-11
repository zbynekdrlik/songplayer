//! #255 (the owner's ruling of 11.11.2026): Dante is ONE audio path and
//! several NICs + a VLAN ONE network layout, so a PC without a Dante leg
//! passes clean. resolume-pp as read on 11.11.2026 (deploy run
//! 42981086181): no Dante output and no `Dante70`; `soundgrid-pcie`
//! (Realtek PCIe, `rt640x64`) disconnected, `Ethernet 4` an RTL8156 on
//! driver 11.4.211.2022 with neither `PendingTransmits` nor
//! `TransmitBufferLen`, up (its LAN, the default route), Wi-Fi
//! disconnected, every firewall profile off. And resolume-snv's RTL8156,
//! the adapter carrying its Dante leg, still judged (dstack two-networks
//! §6, dstack PR 179: only where its driver offers the keyword).

use sp_core::prod_env::EnvProblem;

use super::super::audio::Reading;
use super::super::audio::read::wired::{WiredNicFacts, WiredValue};
use super::super::network::{self, Holder, NetIf, NetworkFacts, PhysicalNic};
use super::super::{Check, EnvFacts, status_of, verdict};

/// PP's `Ethernet 4` (`PnPDeviceID`, 11.11.2026).
const PP_RTL8156: &str = r"USB\VID_0BDA&PID_8156\4F1300131B";

/// The production audio facts with `Ethernet 4` an RTL8156 whose two
/// transmit values read `value`, offered by its driver or not.
fn with_rtl8156(facts: &mut EnvFacts, value: Reading<String>, offered: bool) {
    let nics = facts
        .audio
        .wired_nics
        .as_mut()
        .expect("the example lists them");
    let usb: &mut WiredNicFacts = nics
        .iter_mut()
        .find(|n| n.name.as_deref() == Some("Ethernet 4"))
        .expect("the example has Ethernet 4");
    usb.description = Some("Realtek Gaming USB 2.5GbE Family Controller".to_string());
    usb.device = Some(PP_RTL8156.to_string());
    for keyword in ["PendingTransmits", "TransmitBufferLen"] {
        usb.values.push(WiredValue {
            keyword,
            value: value.clone(),
            offered,
        });
    }
}

fn pp() -> EnvFacts {
    let nic = |index, guid: &str, alias: &str| NetIf {
        index,
        guid: guid.to_string(),
        alias: alias.to_string(),
    };
    let physical = |name: &str, up| PhysicalNic {
        name: name.to_string(),
        enabled: true,
        up,
    };
    let mut facts = EnvFacts {
        network: NetworkFacts {
            dante_ips: Some(Vec::new()),
            interfaces: Some(vec![
                nic(
                    7,
                    "{DBFC2CF4-0000-0000-0000-000000000001}",
                    "soundgrid-pcie",
                ),
                nic(12, "{0B5E0000-0000-0000-0000-000000000002}", "Ethernet 4"),
                nic(15, "{F1000000-0000-4000-8000-000000000003}", "Wi-Fi"),
            ]),
            physical: Some(vec![
                physical("soundgrid-pcie", false),
                physical("Ethernet 4", true),
                physical("Wi-Fi", false),
            ]),
            default_routes: Some(vec![12]),
            legs: Vec::new(),
            bindings: None,
            errors: Vec::new(),
        },
        ..super::production()
    };
    // As PP read it: the two values not in the registry (the driver's
    // keyword list as the check reads it: unread, so taken as offered).
    with_rtl8156(&mut facts, Reading::Absent, true);
    facts
}

#[test]
fn pp_with_no_dante_leg_holds_whatever_its_rtl8156_lacks() {
    let facts = pp();
    assert_eq!(network::leg_carriers(&facts.network), Vec::<String>::new());
    assert_eq!(verdict(&facts).problems, Vec::<EnvProblem>::new());
    let (status, problems) = status_of(&Check::Read(facts), 1);
    assert!(status.ok, "{problems:?}");
    let wired = status.audio.wired_nics.expect("read");
    assert!(wired.iter().all(|n| n.ok == Some(true)), "{wired:?}");
    let usb = wired
        .iter()
        .find(|n| n.name.as_deref() == Some("Ethernet 4"));
    let keywords: Vec<&str> = usb
        .unwrap()
        .values
        .iter()
        .map(|v| v.keyword.as_str())
        .collect();
    assert!(!keywords.contains(&"PendingTransmits"), "{keywords:?}");
    assert!(!keywords.contains(&"TransmitBufferLen"), "{keywords:?}");
    let two = status.two_networks.expect("shown");
    assert_eq!(two.dante_ips, Some(Vec::new()));
}

/// The same RTL8156 at the wrong values with no leg: still nothing.
#[test]
fn pp_s_rtl8156_at_its_drivers_values_is_not_judged_with_no_leg() {
    let mut facts = pp();
    facts.audio.wired_nics.as_mut().unwrap()[1]
        .values
        .truncate(13);
    with_rtl8156(&mut facts, Reading::Value("3".to_string()), true);
    assert!(verdict(&facts).ok);
}

/// resolume-snv: its Dante leg sits on `Dante70`, a VLAN over `Ethernet 4`,
/// the one physical adapter up, which carries it.
#[test]
fn the_adapter_carrying_the_dante_leg_is_judged() {
    let mut snv = super::production();
    assert_eq!(
        network::leg_carriers(&snv.network),
        ["Dante70".to_string(), "Ethernet 4".to_string()]
    );
    with_rtl8156(&mut snv, Reading::Value("3".to_string()), true);
    let codes: Vec<String> = verdict(&snv)
        .problems
        .iter()
        .map(EnvProblem::code)
        .collect();
    assert_eq!(
        codes,
        [
            "wired_nic_value:PendingTransmits",
            "wired_nic_value:TransmitBufferLen"
        ]
    );
    let (status, _) = status_of(&Check::Read(snv.clone()), 1);
    let wired = status.audio.wired_nics.unwrap();
    assert_eq!(wired[1].ok, Some(false));
    assert_eq!(wired[1].values.len(), 15);
    // A driver that offers neither keyword passes (dstack PR 179); one the
    // driver offers and the registry lacks is named.
    let mut lacking = super::production();
    with_rtl8156(&mut lacking, Reading::Absent, false);
    assert!(verdict(&lacking).ok, "{:?}", verdict(&lacking).problems);
    let mut offered = super::production();
    with_rtl8156(&mut offered, Reading::Absent, true);
    let codes: Vec<String> = verdict(&offered)
        .problems
        .iter()
        .map(EnvProblem::code)
        .collect();
    assert_eq!(
        codes,
        [
            "wired_nic_value:PendingTransmits",
            "wired_nic_value:TransmitBufferLen"
        ]
    );
}

/// The carriers: each leg's own interface, and behind a VLAN the one
/// physical adapter up; with two up, which one carries it is not known.
#[test]
fn the_leg_s_carriers_are_its_interface_and_the_one_adapter_up() {
    let snv = network::snv_example();
    let mut two_up = snv.clone();
    two_up.physical = Some(vec![
        PhysicalNic {
            name: "Ethernet 4".to_string(),
            enabled: true,
            up: true,
        },
        PhysicalNic {
            name: "Ethernet".to_string(),
            enabled: true,
            up: true,
        },
    ]);
    assert_eq!(network::leg_carriers(&two_up), ["Dante70".to_string()]);
    let mut disabled_up = two_up.clone();
    disabled_up.physical.as_mut().unwrap()[1].enabled = false;
    assert_eq!(
        network::leg_carriers(&disabled_up),
        ["Dante70".to_string(), "Ethernet 4".to_string()]
    );
    let mut nobody = snv.clone();
    nobody.legs[0].holder = Holder::Nobody;
    nobody.physical = None;
    assert_eq!(network::leg_carriers(&nobody), Vec::<String>::new());
    let unread = NetworkFacts {
        dante_ips: None,
        ..snv.clone()
    };
    assert_eq!(network::leg_carriers(&unread), Vec::<String>::new());
    let no_leg = NetworkFacts {
        dante_ips: Some(Vec::new()),
        ..snv
    };
    assert_eq!(network::leg_carriers(&no_leg), Vec::<String>::new());
}
