//! #255: the audio walk's test registry and PP's / SNV's layout as read on
//! the boxes (12./13.10.2026), shared by `prod_env_audio_read_tests.rs` and
//! `prod_env_audio_read_tests_dvs.rs`.

use std::collections::HashMap;

use sp_core::prod_env::audio::NicSetting;

use super::*;
use crate::prod_env::audio::{NicFacts, Reading, SettingRead};

/// An in-memory HKLM: names compared as Windows does, ASCII case ignored;
/// a value or key never set is absent.
#[derive(Clone, Default)]
pub(super) struct Fake {
    values: HashMap<(String, String), RegValue>,
    keys: HashMap<String, Result<Vec<String>, u32>>,
}

impl Fake {
    pub(super) fn set(&mut self, key: &str, name: &str, value: RegValue) {
        self.values
            .insert((key.to_ascii_lowercase(), name.to_ascii_lowercase()), value);
    }

    pub(super) fn subkeys_of(&mut self, key: &str, subkeys: Result<Vec<String>, u32>) {
        self.keys.insert(key.to_ascii_lowercase(), subkeys);
    }

    /// Lists an adapter instance in the network class (which also holds a
    /// `Properties` key, as on Windows).
    pub(super) fn add_instance(&mut self, index: &str) {
        let list = self
            .keys
            .entry(NET_CLASS.to_ascii_lowercase())
            .or_insert_with(|| Ok(vec!["Properties".to_string()]));
        if let Ok(list) = list {
            list.push(index.to_string());
        }
    }
}

impl RegistryRead for Fake {
    fn value(&self, key: &str, name: &str) -> RegValue {
        self.values
            .get(&(key.to_ascii_lowercase(), name.to_ascii_lowercase()))
            .cloned()
            .unwrap_or(RegValue::Absent)
    }

    fn subkeys(&self, key: &str) -> Result<Vec<String>, u32> {
        self.keys
            .get(&key.to_ascii_lowercase())
            .cloned()
            .unwrap_or(Err(ERROR_FILE_NOT_FOUND))
    }
}

pub(super) fn t(s: &str) -> RegValue {
    RegValue::Text(s.to_string())
}

pub(super) const SG_GUID: &str = "{DBFC2CF4-1A2B-4C3D-8E9F-0123456789AB}";
pub(super) const DVS_GUID: &str = "{0B5E1C2D-3E4F-4A5B-9C6D-7E8F90A1B2C3}";
pub(super) const SG_DEVICE: &str =
    r"PCI\VEN_10EC&DEV_8168&SUBSYS_393917AA&REV_15\01000000684CE00000";
pub(super) const DVS_DEVICE: &str = r"USB\VID_0BDA&PID_8156\000001";
pub(super) const DVS_MAC: &str = "6c1ff773c7d9";

pub(super) fn class_key(index: &str) -> String {
    format!(r"{NET_CLASS}\{index}")
}

pub(super) fn connection(guid: &str) -> String {
    format!(r"{NET_CONNECTIONS}\{guid}\Connection")
}

pub(super) fn device(instance: &str) -> String {
    format!(r"{DEVICE_ENUM}\{instance}")
}

pub(super) fn wdf(service: &str) -> String {
    format!(r"{SERVICES}\{service}\Parameters\Wdf")
}

/// PP's SoundGrid adapter as read on 13.10.2026: Realtek's NDIS `rt640x64`
/// (its `Parameters\Wdf` holds only the WDF versions), instance 0007,
/// every setting off, `PnPCapabilities` 24; the SoundGrid Protocol
/// installed (its service key). Its MAC is `SG_MAC` (the driver's chosen
/// port: [`SG_PREFERENCES`]).
pub(super) fn add_soundgrid(reg: &mut Fake) {
    reg.add_instance("0007");
    let key = class_key("0007");
    reg.set(&key, "NetCfgInstanceId", t(SG_GUID));
    reg.set(&key, "DriverDesc", t("Realtek PCIe GbE Family Controller"));
    reg.set(&key, "DeviceInstanceID", t(SG_DEVICE));
    for keyword in [
        "*InterruptModeration",
        "*FlowControl",
        "*EEE",
        "EnableGreenEthernet",
        "PowerSavingMode",
        "GigaLite",
    ] {
        reg.set(&key, keyword, t("0"));
    }
    reg.set(&key, "PnPCapabilities", RegValue::Dword(24));
    reg.set(&connection(SG_GUID), "Name", t("soundgrid-pcie"));
    reg.set(&device(SG_DEVICE), "Service", t("rt640x64"));
    reg.set(&wdf("rt640x64"), "WdfMajorVersion", RegValue::Dword(1));
    reg.set(&wdf("rt640x64"), "WdfMinorVersion", RegValue::Dword(15));
    reg.subkeys_of(SOUNDGRID_SERVICE, Ok(vec!["Linkage".to_string()]));
}

/// PP's Dante adapter: Realtek's NetAdapterCx `rtucx21x64`
/// (`KmdfLibraryVersion` 1.33), instance 0012 whose id Windows wrote in
/// lower case and which names no device instance (the connection does).
pub(super) fn add_dante(reg: &mut Fake) {
    reg.add_instance("0012");
    let key = class_key("0012");
    reg.set(&key, "NetCfgInstanceId", t(&DVS_GUID.to_ascii_lowercase()));
    reg.set(
        &key,
        "DriverDesc",
        t("Realtek Gaming USB 2.5GbE Family Controller"),
    );
    reg.set(&key, "*IfConnectorPresent", RegValue::Dword(1));
    reg.set(&key, "*InterruptModeration", t("1"));
    reg.set(&key, "*FlowControl", t("3"));
    reg.set(&connection(DVS_GUID), "Name", t("Ethernet 4"));
    reg.set(&connection(DVS_GUID), "PnPInstanceId", t(DVS_DEVICE));
    reg.set(&device(DVS_DEVICE), "Service", t("rtucx21x64"));
    reg.set(&wdf("rtucx21x64"), "KmdfLibraryVersion", t("1.33"));
}

/// Windows Update off with drivers excluded, MMCSS for Pro Audio.
pub(super) fn add_policies(reg: &mut Fake) {
    reg.set(WINDOWS_UPDATE_AU, "NoAutoUpdate", RegValue::Dword(1));
    reg.set(
        WINDOWS_UPDATE,
        "ExcludeWUDriversInQualityUpdate",
        RegValue::Dword(1),
    );
    reg.set(MMCSS_PROFILE, "SystemResponsiveness", RegValue::Dword(0));
    reg.set(
        MMCSS_PROFILE,
        "NetworkThrottlingIndex",
        RegValue::Dword(u32::MAX),
    );
    reg.set(MMCSS_PRO_AUDIO, "Priority", RegValue::Dword(6));
    reg.set(MMCSS_PRO_AUDIO, "Scheduling Category", t("High"));
}

pub(super) fn pp() -> Fake {
    let mut reg = Fake::default();
    add_soundgrid(&mut reg);
    add_dante(&mut reg);
    add_policies(&mut reg);
    reg
}

pub(super) fn row(guid: &str, mac: &str, hardware: bool, filter: bool) -> IfRow {
    IfRow {
        guid: guid.to_string(),
        mac: mac.to_string(),
        permanent_mac: mac.to_string(),
        hardware,
        filter,
    }
}

/// PP's SoundGrid adapter's MAC (`soundgrid-pcie`, 6C-24-08-E5-9C-91).
pub(super) const SG_MAC: &str = "6c2408e59c91";

/// PP's `SoundGrid.Driver.preferences`, verbatim (13.10.2026).
pub(super) const SG_PREFERENCES: &str = r#"<?xml version="1.0"?>
<Preferences>
    <FileFormatVersion>2</FileFormatVersion>
    <Configuration>
        <LocalLanPort>6c:24:08:e5:9c:91</LocalLanPort>
        <IOMode>0</IOMode>
        <NativeDevice></NativeDevice>
        <WdmChannelCount>0</WdmChannelCount>
        <BlockSize>256</BlockSize>
        <ProToolsBufferSize>1024</ProToolsBufferSize>
    </Configuration>
</Preferences>
"#;

/// PP's interfaces: the SoundGrid adapter and its filter module's interface
/// (the same MAC), the Dante adapter and its filter module's interface.
pub(super) fn interfaces() -> Result<Vec<IfRow>, u32> {
    Ok(vec![
        row("{FFFFFFFF-0000-4000-8000-000000000002}", SG_MAC, true, true),
        row(SG_GUID, SG_MAC, true, false),
        row(
            "{FFFFFFFF-0000-4000-8000-000000000001}",
            DVS_MAC,
            true,
            true,
        ),
        row(DVS_GUID, DVS_MAC, true, false),
    ])
}

/// DVS's `manager.json`, with fields the profile does not read.
pub(super) const MANAGER: &str = r#"{"PrimaryInterfaceName":" Ethernet 4 ","PrimaryInterfaceAddress":"6c1ff773c7d9","Latency":"High","AsioLatency":10000,"Rates":{"Sample":96000,"List":[44100,48000]}}"#;

pub(super) fn power_read() -> PowerRead {
    PowerRead {
        scheme: Ok("ab000000-1111-2222-3333-444455556666".to_string()),
        name: Some(" Bitsum Highest Performance ".to_string()),
        min_cores: Ok(Some(100)),
    }
}

pub(super) fn setting(
    setting: NicSetting,
    keyword: Option<&str>,
    value: Reading<String>,
) -> SettingRead {
    SettingRead {
        setting,
        keyword: keyword.map(str::to_string),
        value,
    }
}

pub(super) fn v(s: &str) -> Reading<String> {
    Reading::Value(s.to_string())
}

pub(super) fn soundgrid_facts() -> NicFacts {
    NicFacts {
        guid: SG_GUID.to_string(),
        name: Some("soundgrid-pcie".to_string()),
        description: Some("Realtek PCIe GbE Family Controller".to_string()),
        service: Some("rt640x64".to_string()),
        kmdf_library_version: Reading::Absent,
        netadaptercx_keywords: Some(false),
        settings: vec![
            setting(
                NicSetting::InterruptModeration,
                Some("*InterruptModeration"),
                v("0"),
            ),
            setting(NicSetting::FlowControl, Some("*FlowControl"), v("0")),
            setting(NicSetting::Eee, Some("*EEE"), v("0")),
            setting(
                NicSetting::GreenEthernet,
                Some("EnableGreenEthernet"),
                v("0"),
            ),
            setting(NicSetting::PowerSaving, Some("PowerSavingMode"), v("0")),
            setting(NicSetting::GigaLite, Some("GigaLite"), v("0")),
            setting(
                NicSetting::PowerManagement,
                Some("PnPCapabilities"),
                v("24"),
            ),
        ],
    }
}

pub(super) fn dante_facts() -> NicFacts {
    NicFacts {
        guid: DVS_GUID.to_string(),
        name: Some("Ethernet 4".to_string()),
        description: Some("Realtek Gaming USB 2.5GbE Family Controller".to_string()),
        service: Some("rtucx21x64".to_string()),
        kmdf_library_version: v("1.33"),
        netadaptercx_keywords: Some(true),
        settings: vec![
            setting(
                NicSetting::InterruptModeration,
                Some("*InterruptModeration"),
                v("1"),
            ),
            setting(NicSetting::FlowControl, Some("*FlowControl"), v("3")),
            setting(NicSetting::Eee, None, Reading::Absent),
            setting(NicSetting::GreenEthernet, None, Reading::Absent),
            setting(NicSetting::PowerSaving, None, Reading::Absent),
            setting(NicSetting::GigaLite, None, Reading::Absent),
            setting(NicSetting::PowerManagement, None, Reading::Absent),
        ],
    }
}

pub(super) fn utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}
