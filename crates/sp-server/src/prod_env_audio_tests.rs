//! #255: the audio profile's verdict — the SoundGrid adapter's driver model
//! and settings, Windows Update, MMCSS — and its view on the status (the
//! walk itself: `prod_env_audio_read_tests.rs`).

use sp_core::prod_env::audio::{DriverModel, MmcssSetting, NicSetting, SoundGridState};
use sp_core::prod_env::{EnvProblem, Unreadable};

use super::*;

fn read(setting: NicSetting, value: Reading<String>) -> SettingRead {
    SettingRead {
        setting,
        keyword: None,
        value,
    }
}

fn text(v: &str) -> Reading<String> {
    Reading::Value(v.to_string())
}

/// PP's SoundGrid adapter as it must be.
fn sg_nic() -> NicFacts {
    match production_example().soundgrid {
        SoundGridFacts::Adapter { nic, .. } => nic,
        other => panic!("the fixture's SoundGrid is {other:?}"),
    }
}

fn with_setting(mut nic: NicFacts, setting: NicSetting, value: Reading<String>) -> NicFacts {
    for s in &mut nic.settings {
        if s.setting == setting {
            s.value = value.clone();
        }
    }
    nic
}

fn sg_setting(setting: NicSetting, value: Option<&str>) -> EnvProblem {
    EnvProblem::SoundGridNicSetting {
        nic: "soundgrid-pcie".to_string(),
        setting,
        value: value.map(str::to_string),
    }
}

// ---- the settings (Waves' guide) ----

#[test]
fn only_interrupt_moderation_and_flow_control_are_required() {
    let must_have: Vec<NicSetting> = NicSetting::ALL
        .into_iter()
        .filter(|&s| required(s))
        .collect();
    assert_eq!(
        must_have,
        [NicSetting::InterruptModeration, NicSetting::FlowControl]
    );
}

#[test]
fn every_keyword_setting_holds_only_at_zero() {
    for setting in NicSetting::ALL
        .into_iter()
        .filter(|&s| s != NicSetting::PowerManagement)
    {
        assert!(setting_holds(setting, 0), "{setting:?}");
        assert!(!setting_holds(setting, 1), "{setting:?}");
        assert!(!setting_holds(setting, 3), "{setting:?}");
    }
}

/// `NDIS_DEVICE_DISABLE_PM` (0x08, ndis.h) set = Windows may not turn the
/// adapter off: 24 (the Power Management tab's value) and 280 (KB 2740020)
/// hold; 0, 16 (only `DISABLE_WAKE_UP`), 23 and 256 do not.
#[test]
fn power_management_holds_only_with_the_disable_pm_bit() {
    assert_eq!(NDIS_DEVICE_DISABLE_PM, 0x08);
    let pm = NicSetting::PowerManagement;
    for holds in [8, 24, 280, 0x0F] {
        assert!(setting_holds(pm, holds), "{holds}");
    }
    for allows in [0, 16, 23, 256] {
        assert!(!setting_holds(pm, allows), "{allows}");
    }
}

#[test]
fn a_setting_value_is_read_as_a_number_trimmed() {
    let im = NicSetting::InterruptModeration;
    assert_eq!(setting_verdict(&read(im, text("0"))), SettingVerdict::Holds);
    assert_eq!(
        setting_verdict(&read(im, text(" 0 "))),
        SettingVerdict::Holds
    );
    assert_eq!(
        setting_verdict(&read(im, text("1"))),
        SettingVerdict::Wrong("1".to_string())
    );
    assert_eq!(
        setting_verdict(&read(im, text(" off "))),
        SettingVerdict::Wrong("off".to_string())
    );
    assert_eq!(
        setting_verdict(&read(NicSetting::PowerManagement, text("24"))),
        SettingVerdict::Holds
    );
    assert_eq!(
        setting_verdict(&read(NicSetting::PowerManagement, text("16"))),
        SettingVerdict::Wrong("16".to_string())
    );
    assert_eq!(
        setting_verdict(&read(im, Reading::Unread)),
        SettingVerdict::Unread
    );
}

#[test]
fn a_missing_setting_is_missing_only_when_required_and_power_management_is_zero() {
    let verdicts: Vec<(NicSetting, SettingVerdict)> = NicSetting::ALL
        .into_iter()
        .map(|s| (s, setting_verdict(&read(s, Reading::Absent))))
        .collect();
    assert_eq!(
        verdicts,
        [
            (NicSetting::InterruptModeration, SettingVerdict::Missing),
            (NicSetting::FlowControl, SettingVerdict::Missing),
            (NicSetting::Eee, SettingVerdict::Holds),
            (NicSetting::GreenEthernet, SettingVerdict::Holds),
            (NicSetting::PowerSaving, SettingVerdict::Holds),
            (NicSetting::GigaLite, SettingVerdict::Holds),
            (
                NicSetting::PowerManagement,
                SettingVerdict::Wrong("0".to_string())
            ),
        ]
    );
}

// ---- the driver model ----

#[test]
fn the_driver_model_is_read_from_the_kmdf_registration() {
    assert_eq!(driver_model(&Reading::Absent), Some(DriverModel::Ndis));
    assert_eq!(driver_model(&text("1.33")), Some(DriverModel::NetAdapterCx));
    assert_eq!(driver_model(&Reading::Unread), None);
}

// ---- one SoundGrid adapter ----

#[test]
fn an_adapter_finds_each_setting_by_its_own_name() {
    let nic = with_setting(sg_nic(), NicSetting::FlowControl, text("3"));
    assert_eq!(
        nic.setting(NicSetting::InterruptModeration).value,
        text("0")
    );
    assert_eq!(nic.setting(NicSetting::FlowControl).value, text("3"));
    assert_eq!(nic.setting(NicSetting::PowerManagement).value, text("24"));
    let none = NicFacts::default();
    assert_eq!(
        none.setting(NicSetting::GigaLite),
        SettingRead {
            setting: NicSetting::GigaLite,
            keyword: None,
            value: Reading::Unread,
        }
    );
}

#[test]
fn an_adapter_is_named_by_its_connection_then_its_driver_then_its_guid() {
    let mut nic = sg_nic();
    assert_eq!(nic.label(), "soundgrid-pcie");
    nic.name = None;
    nic.description = Some("Realtek PCIe GbE".to_string());
    assert_eq!(nic.label(), "Realtek PCIe GbE");
    nic.description = None;
    assert_eq!(nic.label(), "{DBFC2CF4-0000-0000-0000-000000000001}");
}

#[test]
fn a_soundgrid_adapter_on_ndis_with_every_setting_off_holds() {
    assert_eq!(nic_problems(&sg_nic()), Vec::<EnvProblem>::new());
}

#[test]
fn a_soundgrid_adapter_on_netadaptercx_is_a_problem() {
    let nic = NicFacts {
        kmdf_library_version: text("1.33"),
        ..sg_nic()
    };
    assert_eq!(
        nic_problems(&nic),
        [EnvProblem::SoundGridNicNetAdapterCx {
            nic: "soundgrid-pcie".to_string()
        }]
    );
}

#[test]
fn a_soundgrid_adapter_that_cannot_be_read_is_a_problem() {
    let unread = EnvProblem::SoundGridNicUnreadable {
        nic: "soundgrid-pcie".to_string(),
    };
    let no_model = NicFacts {
        kmdf_library_version: Reading::Unread,
        ..sg_nic()
    };
    assert_eq!(nic_problems(&no_model), std::slice::from_ref(&unread));
    let no_setting = with_setting(sg_nic(), NicSetting::GigaLite, Reading::Unread);
    assert_eq!(nic_problems(&no_setting), std::slice::from_ref(&unread));
    // An adapter whose instance was not found: nothing read, nothing named
    // twice.
    let nothing = NicFacts {
        guid: "{G}".to_string(),
        ..NicFacts::default()
    };
    assert_eq!(
        nic_problems(&nothing),
        [EnvProblem::SoundGridNicUnreadable {
            nic: "{G}".to_string()
        }]
    );
}

#[test]
fn every_setting_that_is_not_off_is_named_in_order() {
    let mut nic = sg_nic();
    nic = with_setting(nic, NicSetting::InterruptModeration, text("1"));
    nic = with_setting(nic, NicSetting::FlowControl, Reading::Absent);
    nic = with_setting(nic, NicSetting::Eee, text("1"));
    nic = with_setting(nic, NicSetting::GigaLite, Reading::Absent);
    nic = with_setting(nic, NicSetting::PowerManagement, Reading::Absent);
    nic.kmdf_library_version = text("1.33");
    assert_eq!(
        nic_problems(&nic),
        [
            EnvProblem::SoundGridNicNetAdapterCx {
                nic: "soundgrid-pcie".to_string()
            },
            sg_setting(NicSetting::InterruptModeration, Some("1")),
            sg_setting(NicSetting::FlowControl, None),
            sg_setting(NicSetting::Eee, Some("1")),
            sg_setting(NicSetting::PowerManagement, Some("0")),
        ]
    );
    // Unread first, then the model, then the settings.
    nic.kmdf_library_version = Reading::Unread;
    nic = with_setting(nic, NicSetting::PowerSaving, text("2"));
    assert_eq!(
        nic_problems(&nic),
        [
            EnvProblem::SoundGridNicUnreadable {
                nic: "soundgrid-pcie".to_string()
            },
            sg_setting(NicSetting::InterruptModeration, Some("1")),
            sg_setting(NicSetting::FlowControl, None),
            sg_setting(NicSetting::Eee, Some("1")),
            sg_setting(NicSetting::PowerSaving, Some("2")),
            sg_setting(NicSetting::PowerManagement, Some("0")),
        ]
    );
}

// ---- MMCSS ----

fn mmcss_holds() -> MmcssFacts {
    production_example().mmcss
}

fn mmcss(setting: MmcssSetting, value: Option<&str>) -> EnvProblem {
    EnvProblem::Mmcss {
        setting,
        value: value.map(str::to_string),
    }
}

#[test]
fn the_production_mmcss_values_are_pinned() {
    assert_eq!(SYSTEM_RESPONSIVENESS, 0);
    assert_eq!(NETWORK_THROTTLING_OFF, u32::MAX);
    assert_eq!(PRO_AUDIO_PRIORITY, 6);
    assert_eq!(PRO_AUDIO_SCHEDULING, "High");
    assert_eq!(mmcss_problems(&mmcss_holds()), Vec::<EnvProblem>::new());
}

#[test]
fn an_mmcss_value_is_shown_as_windows_names_it() {
    assert_eq!(
        mmcss_shown(MmcssSetting::NetworkThrottlingIndex, 10),
        "0x0000000A"
    );
    assert_eq!(mmcss_shown(MmcssSetting::SystemResponsiveness, 20), "20");
    assert_eq!(mmcss_shown(MmcssSetting::ProAudioPriority, 1), "1");
}

#[test]
fn every_mmcss_value_that_is_not_the_production_one_is_named() {
    let windows_default = MmcssFacts {
        system_responsiveness: Reading::Value(20),
        network_throttling_index: Reading::Value(10),
        pro_audio_priority: Reading::Value(1),
        pro_audio_scheduling: text("Medium"),
    };
    assert_eq!(
        mmcss_problems(&windows_default),
        [
            mmcss(MmcssSetting::SystemResponsiveness, Some("20")),
            mmcss(MmcssSetting::NetworkThrottlingIndex, Some("0x0000000A")),
            mmcss(MmcssSetting::ProAudioPriority, Some("1")),
            mmcss(MmcssSetting::ProAudioSchedulingCategory, Some("Medium")),
        ]
    );
    let missing = MmcssFacts {
        system_responsiveness: Reading::Absent,
        network_throttling_index: Reading::Absent,
        pro_audio_priority: Reading::Absent,
        pro_audio_scheduling: Reading::Absent,
    };
    assert_eq!(
        mmcss_problems(&missing),
        MmcssSetting::ALL.map(|s| mmcss(s, None))
    );
}

#[test]
fn the_scheduling_category_ignores_case_and_padding() {
    for holds in ["High", "high", " HIGH "] {
        let m = MmcssFacts {
            pro_audio_scheduling: text(holds),
            ..mmcss_holds()
        };
        assert_eq!(mmcss_problems(&m), Vec::<EnvProblem>::new(), "{holds:?}");
    }
    let m = MmcssFacts {
        pro_audio_scheduling: text(" Highest "),
        ..mmcss_holds()
    };
    assert_eq!(
        mmcss_problems(&m),
        [mmcss(
            MmcssSetting::ProAudioSchedulingCategory,
            Some("Highest")
        )]
    );
}

#[test]
fn an_mmcss_value_that_cannot_be_read_is_one_problem_first() {
    for unread in 0..4 {
        let mut m = MmcssFacts {
            system_responsiveness: Reading::Value(20),
            ..mmcss_holds()
        };
        match unread {
            0 => m.system_responsiveness = Reading::Unread,
            1 => m.network_throttling_index = Reading::Unread,
            2 => m.pro_audio_priority = Reading::Unread,
            _ => m.pro_audio_scheduling = Reading::Unread,
        }
        let mut expected = vec![EnvProblem::Unreadable(Unreadable::Mmcss)];
        if unread != 0 {
            expected.push(mmcss(MmcssSetting::SystemResponsiveness, Some("20")));
        }
        assert_eq!(mmcss_problems(&m), expected, "unread {unread}");
    }
}

// ---- the whole profile ----

#[test]
fn a_production_profile_holds() {
    assert_eq!(problems(&production_example()), Vec::<EnvProblem>::new());
    // The report-only facts are never a problem: no SoundGrid, no DVS, no
    // power scheme read, core parking on.
    let snv = AudioFacts {
        soundgrid: SoundGridFacts::NotInstalled,
        dante: DanteFacts::default(),
        power: PowerFacts {
            core_parking_min_cores: Some(10),
            ..PowerFacts::default()
        },
        ..production_example()
    };
    assert_eq!(problems(&snv), Vec::<EnvProblem>::new());
}

/// #255 lane 2: both Windows Update policies gate. Either one not read is
/// ONE "cannot be read" first; then automatic updates on, then drivers not
/// excluded.
#[test]
fn windows_update_s_two_policies_gate() {
    let wu = |no_auto_update: Option<bool>, drivers_excluded: Option<bool>| {
        problems(&AudioFacts {
            no_auto_update,
            drivers_excluded,
            ..production_example()
        })
    };
    let unread = EnvProblem::Unreadable(Unreadable::WindowsUpdate);
    assert_eq!(wu(Some(true), Some(true)), Vec::<EnvProblem>::new());
    assert_eq!(
        wu(Some(true), Some(false)),
        [EnvProblem::DriversNotExcluded]
    );
    assert_eq!(wu(Some(false), Some(true)), [EnvProblem::AutoUpdateOn]);
    assert_eq!(
        wu(Some(false), Some(false)),
        [EnvProblem::AutoUpdateOn, EnvProblem::DriversNotExcluded]
    );
    assert_eq!(wu(Some(true), None), std::slice::from_ref(&unread));
    assert_eq!(wu(None, Some(true)), std::slice::from_ref(&unread));
    assert_eq!(wu(None, None), std::slice::from_ref(&unread));
    assert_eq!(
        wu(None, Some(false)),
        [unread.clone(), EnvProblem::DriversNotExcluded]
    );
    assert_eq!(wu(Some(false), None), [unread, EnvProblem::AutoUpdateOn]);
}

#[test]
fn the_profile_names_every_problem_in_order() {
    let nothing = AudioFacts::default();
    assert_eq!(
        problems(&nothing),
        [
            EnvProblem::Unreadable(Unreadable::SoundGrid),
            EnvProblem::Unreadable(Unreadable::WindowsUpdate),
            EnvProblem::Unreadable(Unreadable::Mmcss),
        ]
    );
    let mut bad = production_example();
    bad.soundgrid = SoundGridFacts::Adapter {
        mac: "6c2408e59c91".to_string(),
        nic: NicFacts {
            kmdf_library_version: text("1.33"),
            ..with_setting(sg_nic(), NicSetting::Eee, text("1"))
        },
    };
    bad.no_auto_update = Some(false);
    bad.drivers_excluded = Some(false);
    bad.mmcss.pro_audio_priority = Reading::Value(1);
    assert_eq!(
        problems(&bad),
        [
            EnvProblem::SoundGridNicNetAdapterCx {
                nic: "soundgrid-pcie".to_string()
            },
            sg_setting(NicSetting::Eee, Some("1")),
            EnvProblem::AutoUpdateOn,
            EnvProblem::DriversNotExcluded,
            mmcss(MmcssSetting::ProAudioPriority, Some("1")),
        ]
    );
}

/// A SoundGrid reading and the problems it makes.
type SoundGridCase = (SoundGridFacts, Vec<EnvProblem>);

/// #255: SoundGrid's own problems: none when it is not installed, its port
/// unset or missing, or its read failed; else its adapter's.
#[test]
fn soundgrid_s_problems_follow_its_state() {
    let cases: [SoundGridCase; 5] = [
        (SoundGridFacts::NotInstalled, Vec::new()),
        (
            SoundGridFacts::Unreadable,
            vec![EnvProblem::Unreadable(Unreadable::SoundGrid)],
        ),
        (
            SoundGridFacts::PortUnset,
            vec![EnvProblem::SoundGridPortUnset],
        ),
        (
            SoundGridFacts::PortMissing {
                mac: "6c2408e59c91".to_string(),
            },
            vec![EnvProblem::SoundGridPortMissing {
                mac: "6c2408e59c91".to_string(),
            }],
        ),
        (
            SoundGridFacts::Adapter {
                mac: "6c2408e59c91".to_string(),
                nic: with_setting(sg_nic(), NicSetting::GigaLite, text("1")),
            },
            vec![sg_setting(NicSetting::GigaLite, Some("1"))],
        ),
    ];
    for (soundgrid, expected) in cases {
        assert_eq!(soundgrid_problems(&soundgrid), expected, "{soundgrid:?}");
        // The whole profile lists them first.
        let audio = AudioFacts {
            soundgrid,
            ..production_example()
        };
        assert_eq!(problems(&audio), expected);
    }
    assert_eq!(SoundGridFacts::default(), SoundGridFacts::Unreadable);
}

// ---- the view ----

#[test]
fn a_reading_shows_only_a_value() {
    assert_eq!(text("x").value(), Some("x".to_string()));
    assert_eq!(Reading::<String>::Absent.value(), None);
    assert_eq!(Reading::<String>::Unread.value(), None);
    assert_eq!(Reading::<u32>::default(), Reading::Unread);
}

/// One setting as shown: its name, registry name, value and verdict.
type Shown<'a> = (&'a str, Option<&'a str>, Option<&'a str>, Option<bool>);

#[test]
fn an_adapter_shows_each_setting_and_whether_all_hold() {
    let mut nic = with_setting(sg_nic(), NicSetting::FlowControl, text("1"));
    nic.settings[0].keyword = Some("*InterruptModeration".to_string());
    nic = with_setting(nic, NicSetting::GigaLite, Reading::Absent);
    let view = nic_view(&nic);
    assert_eq!(view.guid, "{DBFC2CF4-0000-0000-0000-000000000001}");
    assert_eq!(view.name.as_deref(), Some("soundgrid-pcie"));
    assert_eq!(view.service.as_deref(), Some("rt640x64"));
    assert_eq!(view.driver_model, Some(DriverModel::Ndis));
    assert_eq!(view.kmdf_library_version, None);
    assert_eq!(view.netadaptercx_keywords, Some(false));
    let shown: Vec<Shown> = view
        .settings
        .iter()
        .map(|s| {
            (
                s.setting.as_str(),
                s.keyword.as_deref(),
                s.value.as_deref(),
                s.ok,
            )
        })
        .collect();
    assert_eq!(
        shown,
        [
            (
                "interrupt_moderation",
                Some("*InterruptModeration"),
                Some("0"),
                Some(true)
            ),
            ("flow_control", None, Some("1"), Some(false)),
            ("eee", None, Some("0"), Some(true)),
            ("green_ethernet", None, Some("0"), Some(true)),
            ("power_saving", None, Some("0"), Some(true)),
            ("gigalite", None, None, Some(true)),
            ("power_management", None, Some("24"), Some(true)),
        ]
    );
    assert_eq!(view.settings_ok, Some(false));
    assert_eq!(nic_view(&sg_nic()).settings_ok, Some(true));
    let unread = with_setting(nic, NicSetting::Eee, Reading::Unread);
    let view = nic_view(&unread);
    assert_eq!(view.settings[2].ok, None);
    assert_eq!(view.settings_ok, None);
    let cx = nic_view(&NicFacts {
        kmdf_library_version: text("1.33"),
        ..sg_nic()
    });
    assert_eq!(cx.driver_model, Some(DriverModel::NetAdapterCx));
    assert_eq!(cx.kmdf_library_version.as_deref(), Some("1.33"));
}

#[test]
fn the_profile_is_shown_with_its_notes() {
    let audio = production_example();
    let p = view(&audio);
    assert_eq!(p.soundgrid, Some(SoundGridState::Found));
    assert_eq!(p.soundgrid_port_mac.as_deref(), Some("6c2408e59c91"));
    assert_eq!(p.soundgrid_nic, Some(nic_view(&sg_nic())));
    assert_eq!(p.dante.installed, Some(true));
    assert_eq!(p.dante.interface_address.as_deref(), Some("6c1ff773c7d9"));
    assert_eq!(p.dante.interface_name.as_deref(), Some("Ethernet 4"));
    let dvs = p.dante.nic.clone().unwrap();
    assert_eq!(dvs.name.as_deref(), Some("Ethernet 4"));
    assert_eq!(dvs.driver_model, Some(DriverModel::NetAdapterCx));
    assert_eq!(p.windows_update.no_auto_update, Some(true));
    assert_eq!(p.windows_update.drivers_excluded, Some(true));
    assert_eq!(p.mmcss.system_responsiveness, Some(0));
    assert_eq!(p.mmcss.network_throttling_index, Some(u32::MAX));
    assert_eq!(p.mmcss.pro_audio_priority, Some(6));
    assert_eq!(
        p.mmcss.pro_audio_scheduling_category.as_deref(),
        Some("High")
    );
    assert_eq!(
        p.power.scheme_guid.as_deref(),
        Some("8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c")
    );
    assert_eq!(p.power.scheme_name.as_deref(), Some("High performance"));
    assert_eq!(p.power.core_parking_min_cores_pct, Some(100));
    assert_eq!(
        p.notes,
        [
            "sieťová karta Dante (DVS) „Ethernet 4“ beží na ovládači NetAdapterCx (vplyv sa ešte \
          meria)"
        ]
    );
    // The driver exclusion missing is a problem now (#255 lane 2), no note.
    let snv_before_setup = view(&AudioFacts {
        drivers_excluded: Some(false),
        ..production_example()
    });
    assert_eq!(snv_before_setup.notes, p.notes);
    assert_eq!(
        snv_before_setup.windows_update.drivers_excluded,
        Some(false)
    );
    // Nothing read: every report-only fact is a note (DVS, the scheme, core
    // parking), SoundGrid unreadable.
    let nothing = view(&AudioFacts::default());
    assert_eq!(nothing.soundgrid, Some(SoundGridState::Unreadable));
    assert_eq!(nothing.soundgrid_nic, None);
    assert_eq!(nothing.dante.nic, None);
    assert_eq!(nothing.notes.len(), 3);
}

/// #255: SoundGrid's state on the status, with the port's MAC when one was
/// chosen and the adapter when it was found.
#[test]
fn soundgrid_is_shown_by_its_state_port_and_adapter() {
    let shown = |soundgrid: SoundGridFacts| {
        let p = view(&AudioFacts {
            soundgrid,
            ..production_example()
        });
        (p.soundgrid, p.soundgrid_port_mac, p.soundgrid_nic)
    };
    assert_eq!(
        shown(SoundGridFacts::NotInstalled),
        (Some(SoundGridState::NotInstalled), None, None)
    );
    assert_eq!(
        shown(SoundGridFacts::Unreadable),
        (Some(SoundGridState::Unreadable), None, None)
    );
    assert_eq!(
        shown(SoundGridFacts::PortUnset),
        (Some(SoundGridState::PortUnset), None, None)
    );
    assert_eq!(
        shown(SoundGridFacts::PortMissing {
            mac: "020000000001".to_string()
        }),
        (
            Some(SoundGridState::PortMissing),
            Some("020000000001".to_string()),
            None
        )
    );
    assert_eq!(
        shown(production_example().soundgrid),
        (
            Some(SoundGridState::Found),
            Some("6c2408e59c91".to_string()),
            Some(nic_view(&sg_nic()))
        )
    );
}
