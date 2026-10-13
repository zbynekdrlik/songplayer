//! #255: the audio profile's walk over an in-memory registry — the raw
//! values, the SoundGrid binding and one adapter, every way a read can fail
//! (Dante, the policies, power and the Windows glue:
//! `prod_env_audio_read_tests_dvs.rs`).

use sp_core::prod_env::audio::NicSetting;

use super::fake::*;
use super::*;
use crate::prod_env::audio::{NicFacts, Reading, SettingRead};

// ---- the raw value ----

#[test]
fn a_value_reads_by_its_registry_type() {
    assert_eq!(reg_value_from(REG_SZ, &utf16le("abc\0zz")), t("abc"));
    assert_eq!(
        reg_value_from(REG_SZ, &utf16le("no terminator")),
        t("no terminator")
    );
    // An odd trailing byte is no UTF-16 unit.
    let mut odd = utf16le("ab");
    odd.push(0x41);
    assert_eq!(reg_value_from(REG_SZ, &odd), t("ab"));
    assert_eq!(
        reg_value_from(REG_EXPAND_SZ, &utf16le(r"%SystemRoot%\x\0")),
        t(r"%SystemRoot%\x")
    );
    assert_eq!(
        reg_value_from(REG_MULTI_SZ, &utf16le("a\0bc\0\0")),
        RegValue::Multi(vec!["a".to_string(), "bc".to_string()])
    );
    assert_eq!(
        reg_value_from(REG_MULTI_SZ, &utf16le("\0")),
        RegValue::Multi(Vec::new())
    );
    assert_eq!(
        reg_value_from(REG_DWORD, &[0x18, 0x01, 0, 0]),
        RegValue::Dword(0x118)
    );
    assert_eq!(
        reg_value_from(REG_DWORD, &[0xFF, 0xFF, 0xFF, 0xFF]),
        RegValue::Dword(u32::MAX)
    );
    assert_eq!(
        reg_value_from(REG_DWORD, &[1, 0]),
        RegValue::Other(REG_DWORD)
    );
    assert_eq!(reg_value_from(3, &[1, 2, 3]), RegValue::Other(3));
    assert_eq!(
        (REG_SZ, REG_EXPAND_SZ, REG_DWORD, REG_MULTI_SZ),
        (1, 2, 4, 7)
    );
    assert_eq!((ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND), (2, 3));
}

/// A setting and the registry names it is looked for under.
type Names = (NicSetting, &'static [&'static str]);

#[test]
fn each_setting_is_looked_for_under_its_names() {
    let table: Vec<Names> = NicSetting::ALL
        .into_iter()
        .map(|s| (s, keywords(s)))
        .collect();
    let expected: [Names; 7] = [
        (NicSetting::InterruptModeration, &["*InterruptModeration"]),
        (NicSetting::FlowControl, &["*FlowControl"]),
        (NicSetting::Eee, &["*EEE"]),
        (
            NicSetting::GreenEthernet,
            &["EnableGreenEthernet", "GreenEthernet"],
        ),
        (
            NicSetting::PowerSaving,
            &["PowerSavingMode", "*PowerSavingMode"],
        ),
        (NicSetting::GigaLite, &["GigaLite"]),
        (NicSetting::PowerManagement, &["PnPCapabilities"]),
    ];
    assert_eq!(table, expected);
}

// ---- the SoundGrid binding ----

#[test]
fn a_guid_is_braced_and_hex_in_its_groups() {
    assert!(is_guid(SG_GUID));
    assert!(is_guid(&SG_GUID.to_ascii_lowercase()));
    for bad in [
        "",
        "{DBFC2CF4-1A2B-4C3D-8E9F-0123456789A}",
        "{DBFC2CF4-1A2B-4C3D-8E9F-0123456789ABC}",
        // One brace replaced, the rest a GUID.
        "(DBFC2CF4-1A2B-4C3D-8E9F-0123456789AB}",
        "{DBFC2CF4-1A2B-4C3D-8E9F-0123456789AB)",
        "{DBFC2CF41-A2B-4C3D-8E9F-0123456789AB}",
        "{DBFC2CF4-1A2B-4C3D-8E9F-01234567-9AB}",
        "{DBFC2CF4-1A2B-4C3D-8E9F-0123456789AG}",
        "{DBFC2CF4-1A2B+4C3D-8E9F-0123456789AB}",
    ] {
        assert!(!is_guid(bad), "{bad:?}");
    }
}

#[test]
fn the_binding_list_names_each_adapter_once() {
    let other = "{11111111-2222-3333-4444-555555555555}";
    let bind = [
        format!(r"\Device\{SG_GUID}"),
        format!(r" \device\{} ", SG_GUID.to_ascii_lowercase()),
        r"\Device\NdisWanIp".to_string(),
        // A GUID behind another prefix of the same length is skipped.
        r"\Network{99999999-2222-3333-4444-555555555555}".to_string(),
        r"\Dev".to_string(),
        format!(r"\DEVICE\{other}"),
    ];
    assert_eq!(bound_guids(&bind), [SG_GUID, other]);
    assert_eq!(bound_guids(&[]), Vec::<String>::new());
}

#[test]
fn an_instance_key_is_four_digits() {
    for yes in ["0000", "0007", "0123"] {
        assert!(is_instance_key(yes), "{yes}");
    }
    for no in ["Properties", "007", "00007", "00a7", ""] {
        assert!(!is_instance_key(no), "{no}");
    }
}

#[test]
fn soundgrid_not_installed_has_no_adapter_and_no_error() {
    for code in [ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND] {
        let mut reg = Fake::default();
        reg.subkeys_of(SOUNDGRID_SERVICE, Err(code));
        let mut errors = Vec::new();
        assert_eq!(
            soundgrid(&reg, &mut errors),
            (Some(false), Some(Vec::new()))
        );
        assert!(errors.is_empty(), "{errors:?}");
    }
}

#[test]
fn soundgrid_bound_to_nothing_lists_no_adapter() {
    let mut reg = Fake::default();
    reg.subkeys_of(SOUNDGRID_SERVICE, Ok(Vec::new()));
    let mut errors = Vec::new();
    assert_eq!(soundgrid(&reg, &mut errors), (Some(true), Some(Vec::new())));
    assert!(errors.is_empty());
}

#[test]
fn a_soundgrid_binding_that_cannot_be_read_says_why() {
    let mut reg = Fake::default();
    reg.subkeys_of(SOUNDGRID_SERVICE, Err(5));
    let mut errors = Vec::new();
    assert_eq!(soundgrid(&reg, &mut errors), (None, None));
    assert_eq!(
        errors,
        ["the SoundGrid Protocol's service could not be read (error 5)"]
    );
    reg.subkeys_of(SOUNDGRID_SERVICE, Ok(Vec::new()));
    reg.set(SOUNDGRID_LINKAGE, "Bind", RegValue::Failed(5));
    let mut errors = Vec::new();
    assert_eq!(soundgrid(&reg, &mut errors), (Some(true), None));
    assert_eq!(
        errors,
        ["the SoundGrid Protocol's bindings could not be read (error 5)"]
    );
    for not_a_list in [RegValue::Dword(1), RegValue::Other(3)] {
        reg.set(SOUNDGRID_LINKAGE, "Bind", not_a_list);
        let mut errors = Vec::new();
        assert_eq!(soundgrid(&reg, &mut errors), (Some(true), None));
        assert_eq!(errors, ["the SoundGrid Protocol's bindings are no list"]);
    }
}

#[test]
fn pp_s_soundgrid_adapter_reads_as_its_facts() {
    let mut errors = Vec::new();
    assert_eq!(
        soundgrid(&pp(), &mut errors),
        (Some(true), Some(vec![soundgrid_facts()]))
    );
    assert!(errors.is_empty(), "{errors:?}");
    // One binding written as a single string reads the same.
    let mut reg = pp();
    reg.set(SOUNDGRID_LINKAGE, "Bind", t(&format!(r"\Device\{SG_GUID}")));
    assert_eq!(
        soundgrid(&reg, &mut errors).1,
        Some(vec![soundgrid_facts()])
    );
}

// ---- one adapter ----

#[test]
fn an_adapter_with_no_device_instance_in_its_class_takes_the_connection_s() {
    let mut errors = Vec::new();
    assert_eq!(nic(&pp(), DVS_GUID, &mut errors), dante_facts());
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn the_class_instance_s_device_wins_over_the_connection_s() {
    let mut reg = pp();
    reg.set(&connection(SG_GUID), "PnPInstanceId", t(DVS_DEVICE));
    let mut errors = Vec::new();
    assert_eq!(
        nic(&reg, SG_GUID, &mut errors).service.as_deref(),
        Some("rt640x64")
    );
}

#[test]
fn an_adapter_missing_from_the_class_says_why() {
    let guid = "{11111111-2222-3333-4444-555555555555}";
    let mut errors = Vec::new();
    let facts = nic(&pp(), guid, &mut errors);
    assert_eq!(
        facts,
        NicFacts {
            guid: guid.to_string(),
            ..NicFacts::default()
        }
    );
    assert_eq!(
        errors,
        [
            format!("the network adapter {guid} has no instance in the network class"),
            format!("the network adapter {guid} names no device instance"),
        ]
    );
    // Instances that could not be read are counted.
    let mut reg = pp();
    reg.add_instance("0013");
    reg.add_instance("0014");
    reg.set(&class_key("0013"), "NetCfgInstanceId", RegValue::Failed(5));
    reg.set(&class_key("0014"), "NetCfgInstanceId", RegValue::Failed(5));
    reg.set(&class_key("0012"), "NetCfgInstanceId", RegValue::Failed(5));
    let mut errors = Vec::new();
    nic(&reg, DVS_GUID, &mut errors);
    assert_eq!(
        errors[0],
        format!(
            "the network adapter {DVS_GUID} was not found in the network class (3 instance(s) \
             could not be read)"
        )
    );
    // The class itself cannot be read.
    reg.subkeys_of(NET_CLASS, Err(5));
    let mut errors = Vec::new();
    nic(&reg, DVS_GUID, &mut errors);
    assert_eq!(
        errors[0],
        "the network adapters' class could not be read (error 5)"
    );
}

#[test]
fn only_instance_keys_are_searched() {
    let mut reg = pp();
    // `Properties` (no instance) holds an id that would match.
    reg.set(&class_key("Properties"), "NetCfgInstanceId", t(SG_GUID));
    reg.set(
        &class_key("0007"),
        "NetCfgInstanceId",
        t("{00000000-0000-0000-0000-000000000000}"),
    );
    let mut errors = Vec::new();
    let facts = nic(&reg, SG_GUID, &mut errors);
    assert_eq!(facts.settings, Vec::<SettingRead>::new());
    assert_eq!(
        errors[0],
        format!("the network adapter {SG_GUID} has no instance in the network class")
    );
}

#[test]
fn an_adapter_s_driver_service_that_cannot_be_read_says_why() {
    let mut reg = pp();
    reg.set(&device(SG_DEVICE), "Service", RegValue::Failed(5));
    let mut errors = Vec::new();
    let facts = nic(&reg, SG_GUID, &mut errors);
    assert_eq!(facts.service, None);
    assert_eq!(facts.kmdf_library_version, Reading::Unread);
    assert_eq!(
        errors,
        [format!(
            "the network adapter {SG_GUID}'s driver service could not be read (error 5)"
        )]
    );
    for none in [RegValue::Absent, t("  "), RegValue::Dword(1)] {
        reg.set(&device(SG_DEVICE), "Service", none);
        let mut errors = Vec::new();
        assert_eq!(nic(&reg, SG_GUID, &mut errors).service, None);
        assert_eq!(
            errors,
            [format!(
                "the network adapter {SG_GUID}'s device names no driver service"
            )]
        );
    }
    let mut reg = pp();
    reg.set(&wdf("rt640x64"), "KmdfLibraryVersion", RegValue::Failed(5));
    let mut errors = Vec::new();
    assert_eq!(
        nic(&reg, SG_GUID, &mut errors).kmdf_library_version,
        Reading::Unread
    );
    assert_eq!(
        errors,
        ["the driver service rt640x64's KMDF registration could not be read (error 5)"]
    );
}

#[test]
fn the_kmdf_registration_of_any_type_is_one() {
    for (value, shown) in [
        (RegValue::Dword(33), "33"),
        (
            RegValue::Multi(vec!["1".to_string(), "33".to_string()]),
            "1 33",
        ),
        (RegValue::Other(3), "(registry type 3)"),
        (t(" 1.33 "), "1.33"),
    ] {
        let mut reg = pp();
        reg.set(&wdf("rt640x64"), "KmdfLibraryVersion", value);
        let mut errors = Vec::new();
        assert_eq!(
            nic(&reg, SG_GUID, &mut errors).kmdf_library_version,
            v(shown)
        );
    }
}

#[test]
fn a_setting_is_taken_from_the_first_of_its_names_that_exists() {
    let mut reg = pp();
    let key = class_key("0007");
    reg.set(&key, "EnableGreenEthernet", RegValue::Absent);
    reg.set(&key, "GreenEthernet", RegValue::Dword(1));
    reg.set(&key, "PowerSavingMode", RegValue::Absent);
    reg.set(&key, "*PowerSavingMode", t("2"));
    reg.set(&key, "*EEE", RegValue::Failed(5));
    reg.set(&key, "PnPCapabilities", RegValue::Absent);
    let mut errors = Vec::new();
    let facts = nic(&reg, SG_GUID, &mut errors);
    assert_eq!(
        facts.setting(NicSetting::GreenEthernet),
        setting(NicSetting::GreenEthernet, Some("GreenEthernet"), v("1"))
    );
    assert_eq!(
        facts.setting(NicSetting::PowerSaving),
        setting(NicSetting::PowerSaving, Some("*PowerSavingMode"), v("2"))
    );
    assert_eq!(
        facts.setting(NicSetting::Eee),
        setting(NicSetting::Eee, Some("*EEE"), Reading::Unread)
    );
    assert_eq!(
        facts.setting(NicSetting::PowerManagement),
        setting(NicSetting::PowerManagement, None, Reading::Absent)
    );
    assert_eq!(
        errors,
        [format!(
            "the network adapter {SG_GUID}'s *EEE could not be read (error 5)"
        )]
    );
    // The first name wins when both exist.
    reg.set(&key, "EnableGreenEthernet", t("0"));
    let facts = nic(&reg, SG_GUID, &mut errors);
    assert_eq!(
        facts.setting(NicSetting::GreenEthernet),
        setting(
            NicSetting::GreenEthernet,
            Some("EnableGreenEthernet"),
            v("0")
        )
    );
}

#[test]
fn the_netadaptercx_keyword_is_read_as_present_absent_or_unread() {
    let mut reg = pp();
    let key = class_key("0012");
    let mut errors = Vec::new();
    assert_eq!(
        nic(&reg, DVS_GUID, &mut errors).netadaptercx_keywords,
        Some(true)
    );
    reg.set(&key, "*IfConnectorPresent", RegValue::Failed(5));
    assert_eq!(nic(&reg, DVS_GUID, &mut errors).netadaptercx_keywords, None);
    reg.set(&key, "*IfConnectorPresent", RegValue::Absent);
    assert_eq!(
        nic(&reg, DVS_GUID, &mut errors).netadaptercx_keywords,
        Some(false)
    );
}

#[test]
fn an_adapter_s_names_are_only_text() {
    let mut reg = pp();
    reg.set(&connection(SG_GUID), "Name", RegValue::Dword(1));
    reg.set(&class_key("0007"), "DriverDesc", t(" "));
    let mut errors = Vec::new();
    let facts = nic(&reg, SG_GUID, &mut errors);
    assert_eq!((facts.name, facts.description), (None, None));
    assert!(errors.is_empty(), "{errors:?}");
}

// ---- no process ----

/// #233/#255: the environment check never spawns a process (a launched
/// process is what Defender scanned on PP): no reader names one. The power
/// scheme is read through the power API, never `powercfg`.
#[test]
fn the_environment_reads_spawn_no_process() {
    let sources = [
        ("prod_env.rs", include_str!("prod_env.rs")),
        ("prod_env_read.rs", include_str!("prod_env_read.rs")),
        ("prod_env_win.rs", include_str!("prod_env_win.rs")),
        ("prod_env_audio.rs", include_str!("prod_env_audio.rs")),
        (
            "prod_env_audio_read.rs",
            include_str!("prod_env_audio_read.rs"),
        ),
        (
            "prod_env_audio_win.rs",
            include_str!("prod_env_audio_win.rs"),
        ),
    ];
    for (file, source) in sources {
        for spawn in [
            "std::process",
            "Command::new",
            "CreateProcess",
            "ShellExecute",
        ] {
            assert!(!source.contains(spawn), "{file} names {spawn}");
        }
    }
}
