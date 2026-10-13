//! #255: the audio profile's vocabulary — the driver models, the adapter
//! settings, the MMCSS values, the adapter's name, the report-only notes and
//! the wire shape.

use super::*;

#[test]
fn the_driver_models_are_named_on_the_wire_and_for_the_operator() {
    assert_eq!(
        serde_json::to_value(DriverModel::Ndis).unwrap(),
        serde_json::json!("ndis")
    );
    assert_eq!(
        serde_json::to_value(DriverModel::NetAdapterCx).unwrap(),
        serde_json::json!("netadaptercx")
    );
    let back: DriverModel = serde_json::from_str("\"netadaptercx\"").unwrap();
    assert_eq!(back, DriverModel::NetAdapterCx);
    assert_eq!(DriverModel::Ndis.name(), "NDIS");
    assert_eq!(DriverModel::NetAdapterCx.name(), "NetAdapterCx");
}

#[test]
fn every_adapter_setting_has_its_name_in_order() {
    let names: Vec<&str> = NicSetting::ALL.iter().map(|s| s.name()).collect();
    assert_eq!(
        names,
        [
            "interrupt_moderation",
            "flow_control",
            "eee",
            "green_ethernet",
            "power_saving",
            "gigalite",
            "power_management",
        ]
    );
    let texts: Vec<&str> = NicSetting::ALL.iter().map(|s| s.name_sk()).collect();
    assert_eq!(
        texts,
        [
            "Interrupt Moderation (moderovanie prerušení)",
            "Flow Control (riadenie toku)",
            "Energy Efficient Ethernet",
            "Green Ethernet",
            "Power Saving Mode (úsporný režim)",
            "GigaLite",
            "vypínanie zariadenia na úsporu energie",
        ]
    );
}

#[test]
fn every_mmcss_value_has_its_name_label_and_production_value() {
    let rows: Vec<(&str, &str, &str)> = MmcssSetting::ALL
        .iter()
        .map(|s| (s.name(), s.label(), s.wanted()))
        .collect();
    assert_eq!(
        rows,
        [
            ("system_responsiveness", "SystemResponsiveness", "0"),
            (
                "network_throttling_index",
                "NetworkThrottlingIndex",
                "0xFFFFFFFF"
            ),
            ("pro_audio_priority", "Priority úlohy Pro Audio", "6"),
            (
                "pro_audio_scheduling_category",
                "Scheduling Category úlohy Pro Audio",
                "High"
            ),
        ]
    );
}

#[test]
fn an_adapter_is_named_by_its_connection_then_its_driver_then_its_guid() {
    assert_eq!(
        nic_label(Some("Ethernet 4"), Some("Realtek USB"), "{G}"),
        "Ethernet 4"
    );
    assert_eq!(nic_label(None, Some("Realtek USB"), "{G}"), "Realtek USB");
    assert_eq!(nic_label(None, None, "{G}"), "{G}");
    let view = NicView {
        guid: "{G}".to_string(),
        description: Some("Realtek USB".to_string()),
        ..NicView::default()
    };
    assert_eq!(view.label(), "Realtek USB");
    let named = NicView {
        name: Some("Dante".to_string()),
        ..view
    };
    assert_eq!(named.label(), "Dante");
}

/// A profile whose report-only facts all hold: DVS on an NDIS adapter, the
/// Windows Update driver exclusion on, a power scheme with no core parking.
fn quiet() -> AudioProfile {
    AudioProfile {
        dante: DanteView {
            installed: Some(true),
            interface_address: Some("6c1ff773c7d9".to_string()),
            interface_name: Some("Ethernet 4".to_string()),
            nic: Some(NicView {
                guid: "{G}".to_string(),
                name: Some("Ethernet 4".to_string()),
                driver_model: Some(DriverModel::Ndis),
                ..NicView::default()
            }),
        },
        windows_update: WindowsUpdateView {
            no_auto_update: Some(true),
            drivers_excluded: Some(true),
        },
        power: PowerView {
            scheme_guid: Some("8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c".to_string()),
            scheme_name: Some("High performance".to_string()),
            core_parking_min_cores_pct: Some(FULL_CORES_PCT),
        },
        ..AudioProfile::default()
    }
}

#[test]
fn a_profile_whose_report_only_facts_hold_has_no_note() {
    assert_eq!(notes(&quiet()), Vec::<String>::new());
    // No DVS at all: nothing to report on it.
    let mut no_dvs = quiet();
    no_dvs.dante = DanteView {
        installed: Some(false),
        ..DanteView::default()
    };
    assert_eq!(notes(&no_dvs), Vec::<String>::new());
    assert_eq!(FULL_CORES_PCT, 100);
}

#[test]
fn the_dante_adapter_on_netadaptercx_is_a_note() {
    let mut p = quiet();
    p.dante.nic.as_mut().unwrap().driver_model = Some(DriverModel::NetAdapterCx);
    assert_eq!(
        notes(&p),
        [
            "sieťová karta Dante (DVS) „Ethernet 4“ beží na ovládači NetAdapterCx (vplyv sa ešte \
          meria)"
        ]
    );
    p.dante.nic.as_mut().unwrap().driver_model = None;
    assert_eq!(
        notes(&p),
        ["ovládač sieťovej karty Dante (DVS) „Ethernet 4“ sa nepodarilo zistiť"]
    );
}

#[test]
fn a_dante_adapter_that_cannot_be_found_is_a_note() {
    let mut p = quiet();
    p.dante.nic = None;
    assert_eq!(
        notes(&p),
        ["sieťovú kartu Dante Virtual Soundcard (MAC 6c1ff773c7d9) sa nepodarilo určiť"]
    );
    p.dante.interface_address = None;
    assert_eq!(
        notes(&p),
        ["sieťovú kartu Dante Virtual Soundcard sa nepodarilo určiť"]
    );
    p.dante.installed = None;
    assert_eq!(
        notes(&p),
        ["inštaláciu Dante Virtual Soundcard sa nepodarilo zistiť"]
    );
}

/// #255 lane 2: the Windows Update driver exclusion is a gate now
/// (`EnvProblem::DriversNotExcluded`, `unreadable:windows_update`), never a
/// note.
#[test]
fn the_windows_update_driver_exclusion_is_no_note() {
    for drivers_excluded in [Some(false), None] {
        let mut p = quiet();
        p.windows_update.drivers_excluded = drivers_excluded;
        assert_eq!(notes(&p), Vec::<String>::new(), "{drivers_excluded:?}");
    }
}

#[test]
fn core_parking_below_every_core_and_an_unread_scheme_are_notes() {
    let mut p = quiet();
    p.power.core_parking_min_cores_pct = Some(99);
    assert_eq!(
        notes(&p),
        ["parkovanie jadier je zapnuté (minimum 99 % jadier)"]
    );
    p.power.core_parking_min_cores_pct = Some(0);
    assert_eq!(
        notes(&p),
        ["parkovanie jadier je zapnuté (minimum 0 % jadier)"]
    );
    p.power.core_parking_min_cores_pct = None;
    assert_eq!(notes(&p), ["parkovanie jadier sa nepodarilo zistiť"]);
    let mut unread = quiet();
    unread.power.scheme_guid = None;
    assert_eq!(
        notes(&unread),
        ["aktívny plán napájania sa nepodarilo zistiť"]
    );
}

#[test]
fn every_note_is_listed_in_order() {
    let p = AudioProfile::default();
    assert_eq!(
        notes(&p),
        [
            "inštaláciu Dante Virtual Soundcard sa nepodarilo zistiť",
            "aktívny plán napájania sa nepodarilo zistiť",
            "parkovanie jadier sa nepodarilo zistiť",
        ]
    );
}

/// The wire shape of one adapter, every field by name.
#[test]
fn an_adapter_serializes_every_field_by_name() {
    let nic = NicView {
        guid: "{DBFC2CF4-0000-0000-0000-000000000000}".to_string(),
        name: Some("soundgrid-pcie".to_string()),
        description: Some("Realtek PCIe GbE Family Controller".to_string()),
        service: Some("rt640x64".to_string()),
        driver_model: Some(DriverModel::Ndis),
        kmdf_library_version: None,
        netadaptercx_keywords: Some(false),
        settings: vec![NicSettingView {
            setting: "interrupt_moderation".to_string(),
            keyword: Some("*InterruptModeration".to_string()),
            value: Some("0".to_string()),
            ok: Some(true),
        }],
        settings_ok: Some(true),
    };
    let json = serde_json::to_value(&nic).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "guid": "{DBFC2CF4-0000-0000-0000-000000000000}",
            "name": "soundgrid-pcie",
            "description": "Realtek PCIe GbE Family Controller",
            "service": "rt640x64",
            "driver_model": "ndis",
            "kmdf_library_version": null,
            "netadaptercx_keywords": false,
            "settings": [{
                "setting": "interrupt_moderation",
                "keyword": "*InterruptModeration",
                "value": "0",
                "ok": true,
            }],
            "settings_ok": true,
        })
    );
    let back: NicView = serde_json::from_value(json).unwrap();
    assert_eq!(back, nic);
}

/// An older server (or the mock) without the profile reads every fact as
/// not known.
#[test]
fn a_missing_profile_reads_as_nothing_known() {
    let p: AudioProfile = serde_json::from_str("{}").unwrap();
    assert_eq!(p, AudioProfile::default());
    assert_eq!(p.soundgrid, None);
    assert_eq!(p.soundgrid_nic, None);
    assert!(p.notes.is_empty());
}

/// #255: SoundGrid's states by name on the wire.
#[test]
fn the_soundgrid_states_are_named_on_the_wire() {
    let names: Vec<serde_json::Value> = [
        SoundGridState::NotInstalled,
        SoundGridState::Unreadable,
        SoundGridState::PortUnset,
        SoundGridState::PortMissing,
        SoundGridState::Found,
    ]
    .iter()
    .map(|s| serde_json::to_value(s).unwrap())
    .collect();
    assert_eq!(
        names,
        [
            "not_installed",
            "unreadable",
            "port_unset",
            "port_missing",
            "found"
        ]
    );
    let back: SoundGridState = serde_json::from_str("\"port_missing\"").unwrap();
    assert_eq!(back, SoundGridState::PortMissing);
}

/// #255: a MAC kept as 12 hex digits reads with colons; anything else as
/// it is.
#[test]
fn a_mac_reads_with_colons() {
    assert_eq!(mac_colons("6c2408e59c91"), "6c:24:08:e5:9c:91");
    for as_is in ["", "6c2408e59c9", "6c2408e59c912", "6c2408e59cé"] {
        assert_eq!(mac_colons(as_is), as_is, "{as_is:?}");
    }
}
