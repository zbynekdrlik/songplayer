//! #255: the audio profile's walk — Dante Virtual Soundcard's
//! configuration and adapter, the interfaces, Windows Update, MMCSS, power,
//! one check's facts — and, on the Windows CI job, the glue against
//! Windows' own reading.

#[cfg(windows)]
use std::collections::HashMap;

#[cfg(windows)]
use sp_core::prod_env::audio::NicSetting;

use super::fake::*;
use super::*;
use crate::prod_env::audio::{
    AudioFacts, DanteFacts, MmcssFacts, PowerFacts, Reading, problems, view,
};

// ---- Dante Virtual Soundcard ----

#[test]
fn dvs_s_configuration_is_read_or_missing_or_says_why() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        read_dvs_file(&dir.path().join("none.json")),
        DvsFile::Missing
    );
    // A directory is no file (Linux reads it and fails; Windows refuses it).
    assert!(matches!(read_dvs_file(dir.path()), DvsFile::Failed(_)));
    let path = dir.path().join("manager.json");
    std::fs::write(&path, MANAGER).unwrap();
    assert_eq!(read_dvs_file(&path), DvsFile::Text(MANAGER.to_string()));
    let at_cap = "a".repeat(MAX_DVS_BYTES as usize);
    std::fs::write(&path, &at_cap).unwrap();
    assert_eq!(read_dvs_file(&path), DvsFile::Text(at_cap));
    std::fs::write(&path, "a".repeat(MAX_DVS_BYTES as usize + 1)).unwrap();
    assert_eq!(
        read_dvs_file(&path),
        DvsFile::Failed("it is larger than 262144 bytes".to_string())
    );
    assert_eq!(MAX_DVS_BYTES, 262_144);
    assert_eq!(
        DVS_MANAGER_JSON,
        r"Audinate\Dante Virtual Soundcard\manager.json"
    );
}

#[test]
fn dvs_s_configuration_names_its_adapter() {
    assert_eq!(
        dvs_config(MANAGER),
        Ok(DvsConfig {
            address: Some(DVS_MAC.to_string()),
            name: Some("Ethernet 4".to_string()),
        })
    );
    // A BOM, a blank name and no address.
    assert_eq!(
        dvs_config("\u{feff}{\"PrimaryInterfaceName\":\" \"}"),
        Ok(DvsConfig {
            address: None,
            name: None,
        })
    );
    let wrong = dvs_config(r#"{"PrimaryInterfaceAddress":5}"#).unwrap_err();
    assert!(wrong.starts_with("Data error at line 1 column "), "{wrong}");
    let broken = dvs_config("{,}").unwrap_err();
    assert!(
        broken.starts_with("Syntax error at line 1 column "),
        "{broken}"
    );
}

#[test]
fn a_mac_is_lower_case_hex_with_no_separators() {
    for written in [
        "6c1ff773c7d9",
        "6C1FF773C7D9",
        " 6c:1f:f7:73:c7:d9 ",
        "6c-1f-f7-73-c7-d9",
        "6c1f.f773.c7d9",
    ] {
        assert_eq!(
            normalize_mac(written).as_deref(),
            Some(DVS_MAC),
            "{written}"
        );
    }
    for no in [
        "",
        "6c1ff773c7d",
        "6c1ff773c7d9a",
        "6c1ff773c7dz",
        "6c 1f f7 73 c7 d9",
    ] {
        assert_eq!(normalize_mac(no), None, "{no:?}");
    }
}

#[test]
fn an_interface_s_flags_and_addresses_read_as_windows_writes_them() {
    assert_eq!((IF_HARDWARE, IF_FILTER), (0x01, 0x02));
    assert_eq!(if_flags(0x00), (false, false));
    assert_eq!(if_flags(0x01), (true, false));
    assert_eq!(if_flags(0x02), (false, true));
    assert_eq!(if_flags(0x03), (true, true));
    assert_eq!(if_flags(0x05), (true, false));
    assert_eq!(if_flags(0xFC), (false, false));
    assert_eq!(mac_hex(&[0x6c, 0x1f, 0xf7, 0x73, 0xc7, 0xd9]), DVS_MAC);
    assert_eq!(mac_hex(&[]), "");
    assert_eq!(
        guid_text(
            0x8c5e_7fda,
            0xe8bf,
            0x4a96,
            [0x9a, 0x85, 0xa6, 0xe2, 0x3a, 0x8c, 0x63, 0x5c]
        ),
        "8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c"
    );
    assert_eq!(
        guid_text(0x0a, 0x0b, 0x0c, [0, 1, 2, 3, 4, 5, 6, 7]),
        "0000000a-000b-000c-0001-020304050607"
    );
}

#[test]
fn the_adapter_with_a_mac_is_a_hardware_one_and_never_a_filter_s() {
    let rows = interfaces().unwrap();
    assert_eq!(adapter_for_mac(&rows, DVS_MAC), Ok(&rows[2]));
    assert_eq!(
        adapter_for_mac(&rows, "000000000000"),
        Err("no network adapter has the MAC 000000000000".to_string())
    );
    // A virtual interface (not hardware) with the MAC is skipped.
    let virtual_one = [row("{V}", DVS_MAC, false, false), rows[2].clone()];
    assert_eq!(adapter_for_mac(&virtual_one, DVS_MAC), Ok(&virtual_one[1]));
    // The permanent address finds an adapter whose address was changed.
    let moved = [IfRow {
        mac: "020000000001".to_string(),
        ..rows[2].clone()
    }];
    assert_eq!(adapter_for_mac(&moved, DVS_MAC), Ok(&moved[0]));
    // The current address wins over another adapter's permanent one.
    let both = [moved[0].clone(), row("{B}", DVS_MAC, true, false)];
    assert_eq!(adapter_for_mac(&both, DVS_MAC), Ok(&both[1]));
    let two = [rows[2].clone(), row("{B}", DVS_MAC, true, false)];
    assert_eq!(
        adapter_for_mac(&two, DVS_MAC),
        Err(format!("2 network adapters have the MAC {DVS_MAC}"))
    );
}

#[test]
fn pp_s_dante_adapter_is_found_by_its_mac() {
    let mut errors = Vec::new();
    assert_eq!(
        dante(
            &pp(),
            &DvsFile::Text(MANAGER.to_string()),
            &interfaces(),
            &mut errors
        ),
        DanteFacts {
            installed: Some(true),
            interface_address: Some(DVS_MAC.to_string()),
            interface_name: Some("Ethernet 4".to_string()),
            nic: Some(dante_facts()),
        }
    );
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn dvs_that_is_not_installed_is_no_error() {
    let mut errors = Vec::new();
    assert_eq!(
        dante(&pp(), &DvsFile::Missing, &interfaces(), &mut errors),
        DanteFacts {
            installed: Some(false),
            ..DanteFacts::default()
        }
    );
    assert!(errors.is_empty());
}

/// A DVS configuration, the interfaces, the facts and the reason.
type DanteCase = (DvsFile, Result<Vec<IfRow>, u32>, DanteFacts, String);

#[test]
fn a_dante_adapter_that_cannot_be_found_says_why() {
    let reg = pp();
    let cases: [DanteCase; 6] = [
        (
            DvsFile::Failed("access denied".to_string()),
            interfaces(),
            DanteFacts::default(),
            "Dante Virtual Soundcard's manager.json could not be read: access denied".to_string(),
        ),
        (
            DvsFile::Text("{,}".to_string()),
            interfaces(),
            DanteFacts {
                installed: Some(true),
                ..DanteFacts::default()
            },
            "Dante Virtual Soundcard's manager.json does not read: Syntax error at line 1 \
             column 2"
                .to_string(),
        ),
        (
            DvsFile::Text(r#"{"PrimaryInterfaceName":"Dante"}"#.to_string()),
            interfaces(),
            DanteFacts {
                installed: Some(true),
                interface_name: Some("Dante".to_string()),
                ..DanteFacts::default()
            },
            "Dante Virtual Soundcard's manager.json names no PrimaryInterfaceAddress".to_string(),
        ),
        (
            DvsFile::Text(r#"{"PrimaryInterfaceAddress":"eth0"}"#.to_string()),
            interfaces(),
            DanteFacts {
                installed: Some(true),
                interface_address: Some("eth0".to_string()),
                ..DanteFacts::default()
            },
            r#"Dante Virtual Soundcard's PrimaryInterfaceAddress "eth0" is no MAC"#.to_string(),
        ),
        (
            DvsFile::Text(MANAGER.to_string()),
            Err(87),
            DanteFacts {
                installed: Some(true),
                interface_address: Some(DVS_MAC.to_string()),
                interface_name: Some("Ethernet 4".to_string()),
                nic: None,
            },
            "the network interfaces could not be listed (error 87)".to_string(),
        ),
        (
            DvsFile::Text(MANAGER.to_string()),
            Ok(Vec::new()),
            DanteFacts {
                installed: Some(true),
                interface_address: Some(DVS_MAC.to_string()),
                interface_name: Some("Ethernet 4".to_string()),
                nic: None,
            },
            format!("Dante Virtual Soundcard's adapter: no network adapter has the MAC {DVS_MAC}"),
        ),
    ];
    for (file, adapters, facts, why) in cases {
        let mut errors = Vec::new();
        assert_eq!(dante(&reg, &file, &adapters, &mut errors), facts, "{why}");
        assert_eq!(errors, [why]);
    }
}

// ---- Windows Update, MMCSS, power ----

#[test]
fn a_policy_is_on_only_as_a_dword_of_one() {
    let mut errors = Vec::new();
    assert_eq!(policy_on(RegValue::Dword(1), "p", &mut errors), Some(true));
    for off in [
        RegValue::Dword(0),
        RegValue::Dword(2),
        RegValue::Absent,
        t("1"),
        RegValue::Multi(vec!["1".to_string()]),
        RegValue::Other(11),
    ] {
        assert_eq!(
            policy_on(off.clone(), "p", &mut errors),
            Some(false),
            "{off:?}"
        );
    }
    assert!(errors.is_empty());
    assert_eq!(policy_on(RegValue::Failed(5), "p", &mut errors), None);
    assert_eq!(errors, ["p could not be read (error 5)"]);
}

#[test]
fn mmcss_reads_its_values_and_says_why_one_cannot_be() {
    let mut errors = Vec::new();
    assert_eq!(
        mmcss(&pp(), &mut errors),
        MmcssFacts {
            system_responsiveness: Reading::Value(0),
            network_throttling_index: Reading::Value(u32::MAX),
            pro_audio_priority: Reading::Value(6),
            pro_audio_scheduling: v("High"),
        }
    );
    assert!(errors.is_empty());
    let mut reg = Fake::default();
    reg.set(MMCSS_PROFILE, "SystemResponsiveness", t("0"));
    reg.set(MMCSS_PROFILE, "NetworkThrottlingIndex", RegValue::Failed(5));
    reg.set(MMCSS_PRO_AUDIO, "Scheduling Category", RegValue::Dword(1));
    let mut errors = Vec::new();
    assert_eq!(
        mmcss(&reg, &mut errors),
        MmcssFacts {
            system_responsiveness: Reading::Unread,
            network_throttling_index: Reading::Unread,
            pro_audio_priority: Reading::Absent,
            pro_audio_scheduling: Reading::Unread,
        }
    );
    assert_eq!(
        errors,
        [
            "MMCSS's SystemResponsiveness is no DWORD",
            "MMCSS's NetworkThrottlingIndex could not be read (error 5)",
            "MMCSS's Pro Audio Scheduling Category is no string",
        ]
    );
    reg.set(MMCSS_PRO_AUDIO, "Scheduling Category", RegValue::Failed(2));
    reg.set(MMCSS_PRO_AUDIO, "Priority", RegValue::Multi(Vec::new()));
    let mut errors = Vec::new();
    let facts = mmcss(&reg, &mut errors);
    assert_eq!(facts.pro_audio_scheduling, Reading::Unread);
    assert_eq!(facts.pro_audio_priority, Reading::Unread);
    assert_eq!(
        errors[2..],
        [
            "MMCSS's Pro Audio Priority is no DWORD",
            "MMCSS's Pro Audio Scheduling Category could not be read (error 2)",
        ]
    );
}

#[test]
fn the_power_reads_become_facts_or_say_why() {
    let mut errors = Vec::new();
    assert_eq!(
        power_facts(&power_read(), &mut errors),
        PowerFacts {
            scheme_guid: Some("ab000000-1111-2222-3333-444455556666".to_string()),
            scheme_name: Some("Bitsum Highest Performance".to_string()),
            core_parking_min_cores: Some(100),
        }
    );
    assert!(errors.is_empty());
    let unset = PowerRead {
        name: Some("  ".to_string()),
        min_cores: Ok(None),
        ..power_read()
    };
    let facts = power_facts(&unset, &mut errors);
    assert_eq!(
        (facts.scheme_name, facts.core_parking_min_cores),
        (None, None)
    );
    assert!(errors.is_empty());
    let failed = PowerRead {
        scheme: Err(5),
        name: None,
        min_cores: Err(87),
    };
    assert_eq!(power_facts(&failed, &mut errors), PowerFacts::default());
    assert_eq!(
        errors,
        [
            "the active power scheme could not be read (error 5)",
            "the core-parking minimum could not be read (error 87)",
        ]
    );
}

// ---- one check's audio facts ----

#[test]
fn pp_s_profile_reads_as_its_facts_and_holds() {
    let facts = read_audio(
        &pp(),
        &interfaces(),
        &DvsFile::Text(MANAGER.to_string()),
        &power_read(),
    );
    assert_eq!(
        facts,
        AudioFacts {
            soundgrid_installed: Some(true),
            soundgrid_nics: Some(vec![soundgrid_facts()]),
            dante: DanteFacts {
                installed: Some(true),
                interface_address: Some(DVS_MAC.to_string()),
                interface_name: Some("Ethernet 4".to_string()),
                nic: Some(dante_facts()),
            },
            no_auto_update: Some(true),
            drivers_excluded: Some(true),
            mmcss: MmcssFacts {
                system_responsiveness: Reading::Value(0),
                network_throttling_index: Reading::Value(u32::MAX),
                pro_audio_priority: Reading::Value(6),
                pro_audio_scheduling: v("High"),
            },
            power: PowerFacts {
                scheme_guid: Some("ab000000-1111-2222-3333-444455556666".to_string()),
                scheme_name: Some("Bitsum Highest Performance".to_string()),
                core_parking_min_cores: Some(100),
            },
            errors: Vec::new(),
        }
    );
    assert!(problems(&facts).is_empty());
    assert_eq!(
        view(&facts).notes,
        [
            "sieťová karta Dante (DVS) „Ethernet 4“ beží na ovládači NetAdapterCx (vplyv sa ešte \
          meria)"
        ]
    );
}

/// SNV: no SoundGrid, DVS on its own USB adapter, the Windows Update driver
/// exclusion missing (lane 2 applies it): no problem, two notes.
#[test]
fn snv_s_profile_holds_with_notes() {
    let mut reg = Fake::default();
    add_dante(&mut reg);
    add_policies(&mut reg);
    reg.set(
        WINDOWS_UPDATE,
        "ExcludeWUDriversInQualityUpdate",
        RegValue::Absent,
    );
    let manager = r#"{"PrimaryInterfaceName":"Dante","PrimaryInterfaceAddress":"00e04c02d461"}"#;
    let facts = read_audio(
        &reg,
        &Ok(vec![row(DVS_GUID, "00e04c02d461", true, false)]),
        &DvsFile::Text(manager.to_string()),
        &power_read(),
    );
    assert!(facts.errors.is_empty(), "{:?}", facts.errors);
    assert_eq!(facts.soundgrid_installed, Some(false));
    assert_eq!(facts.soundgrid_nics, Some(Vec::new()));
    assert_eq!(facts.drivers_excluded, Some(false));
    assert!(problems(&facts).is_empty());
    assert_eq!(view(&facts).notes.len(), 2);
}

#[test]
fn a_profile_that_cannot_be_read_says_why_once_per_reason() {
    let mut reg = Fake::default();
    reg.subkeys_of(SOUNDGRID_SERVICE, Err(5));
    reg.set(WINDOWS_UPDATE_AU, "NoAutoUpdate", RegValue::Failed(5));
    reg.set(
        WINDOWS_UPDATE,
        "ExcludeWUDriversInQualityUpdate",
        RegValue::Failed(5),
    );
    let facts = read_audio(
        &reg,
        &Err(87),
        &DvsFile::Missing,
        &PowerRead {
            scheme: Err(2),
            name: None,
            min_cores: Err(2),
        },
    );
    assert_eq!(facts.soundgrid_nics, None);
    assert_eq!(facts.no_auto_update, None);
    assert_eq!(facts.drivers_excluded, None);
    assert_eq!(
        facts.errors,
        [
            "the SoundGrid Protocol's service could not be read (error 5)",
            "Windows Update's NoAutoUpdate could not be read (error 5)",
            "Windows Update's ExcludeWUDriversInQualityUpdate could not be read (error 5)",
            "the active power scheme could not be read (error 2)",
            "the core-parking minimum could not be read (error 2)",
        ]
    );
}

// ---- the Windows glue, on the Windows CI job ----

/// One reading the CI step "Read the runner's Defender, firewall, build and
/// audio profile for the glue tests" (`ci.yml`, Build (Windows)) wrote:
/// Windows' own cmdlets' answer, independent of the glue.
#[cfg(windows)]
fn expected(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!("{name} is set by ci.yml's step before the tests (Windows' own reading)")
    })
}

/// `name=value;…` pairs the step wrote (an empty value: none).
#[cfg(windows)]
fn pairs(text: &str) -> HashMap<String, String> {
    text.split(';')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

/// A DWORD the step wrote (PowerShell reads a DWORD as a signed Int32).
#[cfg(windows)]
fn dword(pairs: &HashMap<String, String>, name: &str) -> Reading<u32> {
    match pairs.get(name).map(String::as_str) {
        None | Some("") => Reading::Absent,
        Some(text) => Reading::Value(
            text.parse::<i64>()
                .unwrap_or_else(|_| panic!("{name} reads {text:?}")) as u32,
        ),
    }
}

/// #255: the audio glue (registry, interfaces, power API) on a real Windows
/// (the CI runner), each fact against Windows' own reading: MMCSS and
/// Windows Update by `Get-ItemProperty`, the power scheme and its
/// core-parking minimum by `powercfg`, one adapter by `Win32_NetworkAdapter`
/// (its GUID, service, MAC and connection name) and its service's
/// `Parameters\Wdf`. The runner has neither SoundGrid nor DVS. Its reads go
/// to the job's log.
#[cfg(windows)]
#[test]
fn the_audio_glue_reads_what_windows_itself_reports() {
    use std::io::Write;

    use super::super::win;

    let audio = win::read_audio();
    let _ = writeln!(
        std::io::stderr(),
        "[#255 audio profile on the runner] {audio:?}"
    );
    assert!(audio.errors.is_empty(), "{:?}", audio.errors);
    assert_eq!(audio.soundgrid_installed, Some(false));
    assert_eq!(audio.soundgrid_nics, Some(Vec::new()));
    assert_eq!(audio.dante.installed, Some(false));

    let mmcss = pairs(&expected("SP_ENV_EXPECT_MMCSS"));
    assert_eq!(
        audio.mmcss.system_responsiveness,
        dword(&mmcss, "SystemResponsiveness"),
        "SystemResponsiveness"
    );
    assert_eq!(
        audio.mmcss.network_throttling_index,
        dword(&mmcss, "NetworkThrottlingIndex"),
        "NetworkThrottlingIndex"
    );
    assert_eq!(
        audio.mmcss.pro_audio_priority,
        dword(&mmcss, "Priority"),
        "Pro Audio Priority"
    );
    let scheduling = match mmcss.get("SchedulingCategory").map(String::as_str) {
        None | Some("") => Reading::Absent,
        Some(text) => v(text),
    };
    assert_eq!(audio.mmcss.pro_audio_scheduling, scheduling);

    let wu = pairs(&expected("SP_ENV_EXPECT_WU"));
    let on = |name: &str| Some(wu.get(name).map(String::as_str) == Some("1"));
    assert_eq!(audio.no_auto_update, on("NoAutoUpdate"));
    assert_eq!(
        audio.drivers_excluded,
        on("ExcludeWUDriversInQualityUpdate")
    );

    let power = expected("SP_ENV_EXPECT_POWER");
    let (scheme, min) = power
        .split_once(';')
        .expect("SP_ENV_EXPECT_POWER is guid;min");
    assert_eq!(
        audio.power.scheme_guid.as_deref(),
        Some(scheme.trim().to_ascii_lowercase().as_str()),
        "powercfg /getactivescheme"
    );
    let min = min.trim();
    let min = (!min.is_empty()).then(|| min.parse::<u32>().expect("a percent"));
    assert_eq!(
        audio.power.core_parking_min_cores, min,
        "powercfg /qh CPMINCORES"
    );

    let line = expected("SP_ENV_EXPECT_NIC");
    let parts: Vec<&str> = line.splitn(5, ';').map(str::trim).collect();
    let [guid, service, mac, kmdf, name] = parts[..] else {
        panic!("SP_ENV_EXPECT_NIC {line:?} is guid;service;mac;kmdf;name");
    };
    let mut errors = Vec::new();
    let facts = nic(&win::WinRegistry, guid, &mut errors);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(
        facts.service.as_deref().map(str::to_ascii_lowercase),
        Some(service.to_ascii_lowercase()),
        "Win32_NetworkAdapter.ServiceName"
    );
    assert_eq!(facts.name.as_deref().unwrap_or(""), name, "NetConnectionID");
    let registered = matches!(facts.kmdf_library_version, Reading::Value(_));
    assert_eq!(
        registered,
        kmdf.eq_ignore_ascii_case("true"),
        "Parameters\\Wdf\\KmdfLibraryVersion"
    );
    assert_eq!(facts.settings.len(), NicSetting::ALL.len());
    let rows = win::if_table().expect("GetIfTable2");
    let mac = normalize_mac(mac).expect("Win32_NetworkAdapter.MACAddress");
    assert!(
        rows.iter()
            .any(|r| r.guid.eq_ignore_ascii_case(guid) && r.mac == mac),
        "{guid} {mac} in {rows:?}"
    );
}
