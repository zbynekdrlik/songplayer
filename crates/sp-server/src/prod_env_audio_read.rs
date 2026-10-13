//! #255: the audio profile's reads made into facts (pure, Linux-tested;
//! `prod_env_audio_win.rs` only reads).
//!
//! The registry is walked through [`RegistryRead`] (two primitives: a value,
//! a key's subkeys), which the Windows glue implements with `RegGetValueW` /
//! `RegEnumKeyExW` and the tests with an in-memory registry. An audio
//! adapter is FOUND by what binds it, never by its name:
//!
//! - the SoundGrid Protocol's `Linkage\Bind` lists `\Device\{GUID}` per
//!   adapter it is bound to ([`bound_guids`]);
//! - Dante Virtual Soundcard's `manager.json` names its adapter's MAC
//!   (`PrimaryInterfaceAddress`), matched against the network interfaces
//!   (`GetIfTable2`, [`adapter_for_mac`]).
//!
//! From the GUID ([`nic`]): the adapter's instance in the network class
//! (`NetCfgInstanceId`) holds its settings; its device instance
//! (`DeviceInstanceID` there, else the connection's `PnPInstanceId`) names
//! its driver `Service`; that service's `Parameters\Wdf\KmdfLibraryVersion`
//! tells NetAdapterCx from NDIS (`super::driver_model`).

use std::io::Read;
use std::path::Path;

use serde::Deserialize;
use sp_core::prod_env::audio::NicSetting;

use super::{AudioFacts, DanteFacts, MmcssFacts, NicFacts, PowerFacts, Reading, SettingRead};

/// A registry value as read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegValue {
    Dword(u32),
    /// `REG_SZ` / `REG_EXPAND_SZ` (not expanded).
    Text(String),
    /// `REG_MULTI_SZ`.
    Multi(Vec<String>),
    /// Another type (its `REG_*` number).
    Other(u32),
    /// The key or the value does not exist.
    Absent,
    /// The read failed with this Win32 error.
    Failed(u32),
}

/// The registry under `HKEY_LOCAL_MACHINE`, as the walk reads it.
pub trait RegistryRead {
    /// One value of a key.
    fn value(&self, key: &str, name: &str) -> RegValue;
    /// A key's subkeys' names; `Err(ERROR_FILE_NOT_FOUND)` when there is no
    /// such key.
    fn subkeys(&self, key: &str) -> Result<Vec<String>, u32>;
}

/// `ERROR_FILE_NOT_FOUND`: no such key or value.
pub const ERROR_FILE_NOT_FOUND: u32 = 2;
/// `ERROR_PATH_NOT_FOUND`: no such key.
pub const ERROR_PATH_NOT_FOUND: u32 = 3;

/// `REG_SZ`.
pub const REG_SZ: u32 = 1;
/// `REG_EXPAND_SZ`.
pub const REG_EXPAND_SZ: u32 = 2;
/// `REG_DWORD`.
pub const REG_DWORD: u32 = 4;
/// `REG_MULTI_SZ`.
pub const REG_MULTI_SZ: u32 = 7;

/// A value's bytes as `RegGetValueW` hands them over, by their type: a
/// string up to its NUL, a list of strings, a little-endian DWORD.
pub fn reg_value_from(kind: u32, bytes: &[u8]) -> RegValue {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    match kind {
        REG_SZ | REG_EXPAND_SZ => {
            let end = units.iter().position(|&c| c == 0).unwrap_or(units.len());
            RegValue::Text(String::from_utf16_lossy(&units[..end]))
        }
        REG_MULTI_SZ => RegValue::Multi(
            units
                .split(|&c| c == 0)
                .filter(|s| !s.is_empty())
                .map(String::from_utf16_lossy)
                .collect(),
        ),
        REG_DWORD => match <[u8; 4]>::try_from(bytes) {
            Ok(b) => RegValue::Dword(u32::from_le_bytes(b)),
            Err(_) => RegValue::Other(kind),
        },
        other => RegValue::Other(other),
    }
}

/// The SoundGrid Protocol's service key.
pub const SOUNDGRID_SERVICE: &str = r"SYSTEM\CurrentControlSet\Services\SoundGridProtocol";
/// Its bindings (`Bind`: `\Device\{GUID}` per adapter).
pub const SOUNDGRID_LINKAGE: &str = r"SYSTEM\CurrentControlSet\Services\SoundGridProtocol\Linkage";
/// The network adapters' class (`<nnnn>` per adapter instance).
pub const NET_CLASS: &str =
    r"SYSTEM\CurrentControlSet\Control\Class\{4d36e972-e325-11ce-bfc1-08002be10318}";
/// The network connections (`{GUID}\Connection`: `Name`, `PnPInstanceId`).
pub const NET_CONNECTIONS: &str =
    r"SYSTEM\CurrentControlSet\Control\Network\{4D36E972-E325-11CE-BFC1-08002BE10318}";
/// The device instances (`<instance>`: `Service`).
pub const DEVICE_ENUM: &str = r"SYSTEM\CurrentControlSet\Enum";
/// The services (`<service>\Parameters\Wdf`).
pub const SERVICES: &str = r"SYSTEM\CurrentControlSet\Services";
/// Windows Update's policies (`ExcludeWUDriversInQualityUpdate`).
pub const WINDOWS_UPDATE: &str = r"SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate";
/// Windows Update's automatic-update policies (`NoAutoUpdate`).
pub const WINDOWS_UPDATE_AU: &str = r"SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU";
/// MMCSS's system profile.
pub const MMCSS_PROFILE: &str =
    r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Multimedia\SystemProfile";
/// MMCSS's Pro Audio task.
pub const MMCSS_PRO_AUDIO: &str =
    r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Multimedia\SystemProfile\Tasks\Pro Audio";
/// Dante Virtual Soundcard's configuration, under `%ProgramData%`.
pub const DVS_MANAGER_JSON: &str = r"Audinate\Dante Virtual Soundcard\manager.json";
/// The most of `manager.json` that is read (it is a few KB).
pub const MAX_DVS_BYTES: u64 = 262_144;

/// The registry values a setting is found under, in the order tried (the
/// standardized `*` keyword first where one exists; Realtek's own names).
pub fn keywords(setting: NicSetting) -> &'static [&'static str] {
    match setting {
        NicSetting::InterruptModeration => &["*InterruptModeration"],
        NicSetting::FlowControl => &["*FlowControl"],
        NicSetting::Eee => &["*EEE"],
        NicSetting::GreenEthernet => &["EnableGreenEthernet", "GreenEthernet"],
        NicSetting::PowerSaving => &["PowerSavingMode", "*PowerSavingMode"],
        NicSetting::GigaLite => &["GigaLite"],
        NicSetting::PowerManagement => &["PnPCapabilities"],
    }
}

/// A braced GUID (`{8-4-4-4-12}`, hex digits).
pub fn is_guid(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 38 || b[0] != b'{' || b[37] != b'}' {
        return false;
    }
    b[1..37].iter().enumerate().all(|(i, &c)| {
        if matches!(i, 8 | 13 | 18 | 23) {
            c == b'-'
        } else {
            c.is_ascii_hexdigit()
        }
    })
}

/// The adapters' GUIDs in a binding list (`\Device\{GUID}`, case ignored),
/// each once, in order; any other entry is skipped.
pub fn bound_guids(bind: &[String]) -> Vec<String> {
    const DEVICE: &str = r"\Device\";
    let mut guids: Vec<String> = Vec::new();
    for entry in bind {
        let entry = entry.trim();
        let Some(head) = entry.get(..DEVICE.len()) else {
            continue;
        };
        let guid = &entry[DEVICE.len()..];
        if head.eq_ignore_ascii_case(DEVICE)
            && is_guid(guid)
            && !guids.iter().any(|g| g.eq_ignore_ascii_case(guid))
        {
            guids.push(guid.to_string());
        }
    }
    guids
}

/// An adapter instance's key name in the network class (`0007`); the class
/// key also holds `Properties` and others.
pub fn is_instance_key(name: &str) -> bool {
    name.len() == 4 && name.bytes().all(|b| b.is_ascii_digit())
}

/// A string value, trimmed; `None` for anything else or an empty one.
fn text_of(value: RegValue) -> Option<String> {
    match value {
        RegValue::Text(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

/// Whether a value exists (`None`: could not be read).
fn presence(value: RegValue) -> Option<bool> {
    match value {
        RegValue::Absent => Some(false),
        RegValue::Failed(_) => None,
        _ => Some(true),
    }
}

/// A value as a reading of text: absent, its text, or the read's error.
fn reading_of(value: RegValue) -> Result<Reading<String>, u32> {
    Ok(match value {
        RegValue::Absent => Reading::Absent,
        RegValue::Failed(code) => return Err(code),
        RegValue::Dword(n) => Reading::Value(n.to_string()),
        RegValue::Text(s) => Reading::Value(s.trim().to_string()),
        RegValue::Multi(list) => Reading::Value(list.join(" ")),
        RegValue::Other(kind) => Reading::Value(format!("(registry type {kind})")),
    })
}

/// The SoundGrid Protocol: installed (its service key exists), and every
/// adapter it is bound to (`None`: the binding could not be read).
pub fn soundgrid(
    reg: &dyn RegistryRead,
    errors: &mut Vec<String>,
) -> (Option<bool>, Option<Vec<NicFacts>>) {
    match reg.subkeys(SOUNDGRID_SERVICE) {
        Ok(_) => {}
        Err(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) => return (Some(false), Some(Vec::new())),
        Err(code) => {
            errors.push(format!(
                "the SoundGrid Protocol's service could not be read (error {code})"
            ));
            return (None, None);
        }
    }
    let bind = match reg.value(SOUNDGRID_LINKAGE, "Bind") {
        RegValue::Multi(list) => list,
        RegValue::Text(entry) => vec![entry],
        RegValue::Absent => Vec::new(),
        RegValue::Failed(code) => {
            errors.push(format!(
                "the SoundGrid Protocol's bindings could not be read (error {code})"
            ));
            return (Some(true), None);
        }
        RegValue::Dword(_) | RegValue::Other(_) => {
            errors.push("the SoundGrid Protocol's bindings are no list".to_string());
            return (Some(true), None);
        }
    };
    let nics = bound_guids(&bind)
        .iter()
        .map(|guid| nic(reg, guid, errors))
        .collect();
    (Some(true), Some(nics))
}

/// The adapter's instance key in the network class (its `NetCfgInstanceId`
/// is `guid`, case ignored), or `None` and why.
fn class_instance(reg: &dyn RegistryRead, guid: &str, errors: &mut Vec<String>) -> Option<String> {
    let names = match reg.subkeys(NET_CLASS) {
        Ok(names) => names,
        Err(code) => {
            errors.push(format!(
                "the network adapters' class could not be read (error {code})"
            ));
            return None;
        }
    };
    let mut failed = 0usize;
    for name in names.iter().filter(|n| is_instance_key(n)) {
        let key = format!(r"{NET_CLASS}\{name}");
        match reg.value(&key, "NetCfgInstanceId") {
            RegValue::Text(id) if id.trim().eq_ignore_ascii_case(guid.trim()) => return Some(key),
            RegValue::Failed(_) => failed += 1,
            _ => {}
        }
    }
    errors.push(if failed == 0 {
        format!("the network adapter {guid} has no instance in the network class")
    } else {
        format!(
            "the network adapter {guid} was not found in the network class ({failed} instance(s) \
             could not be read)"
        )
    });
    None
}

/// One setting from the adapter's instance key: the first of its
/// [`keywords`] that exists.
fn setting_read(
    reg: &dyn RegistryRead,
    key: &str,
    setting: NicSetting,
    guid: &str,
    errors: &mut Vec<String>,
) -> SettingRead {
    for &keyword in keywords(setting) {
        let value = match reading_of(reg.value(key, keyword)) {
            Ok(Reading::Absent) => continue,
            Ok(value) => value,
            Err(code) => {
                errors.push(format!(
                    "the network adapter {guid}'s {keyword} could not be read (error {code})"
                ));
                Reading::Unread
            }
        };
        return SettingRead {
            setting,
            keyword: Some(keyword.to_string()),
            value,
        };
    }
    SettingRead {
        setting,
        keyword: None,
        value: Reading::Absent,
    }
}

/// One adapter by its GUID: its names, settings, driver service and the
/// service's KMDF registration. What cannot be read stays unread, and says
/// why.
pub fn nic(reg: &dyn RegistryRead, guid: &str, errors: &mut Vec<String>) -> NicFacts {
    let mut nic = NicFacts {
        guid: guid.to_string(),
        ..NicFacts::default()
    };
    let connection = format!(r"{NET_CONNECTIONS}\{guid}\Connection");
    nic.name = text_of(reg.value(&connection, "Name"));
    let mut instance = None;
    if let Some(key) = class_instance(reg, guid, errors) {
        nic.description = text_of(reg.value(&key, "DriverDesc"));
        instance = text_of(reg.value(&key, "DeviceInstanceID"));
        nic.netadaptercx_keywords = presence(reg.value(&key, "*IfConnectorPresent"));
        nic.settings = NicSetting::ALL
            .iter()
            .map(|&s| setting_read(reg, &key, s, guid, errors))
            .collect();
    }
    let Some(instance) = instance.or_else(|| text_of(reg.value(&connection, "PnPInstanceId")))
    else {
        errors.push(format!(
            "the network adapter {guid} names no device instance"
        ));
        return nic;
    };
    let device = format!(r"{DEVICE_ENUM}\{instance}");
    nic.service = match reg.value(&device, "Service") {
        RegValue::Failed(code) => {
            errors.push(format!(
                "the network adapter {guid}'s driver service could not be read (error {code})"
            ));
            None
        }
        value => {
            let service = text_of(value);
            if service.is_none() {
                errors.push(format!(
                    "the network adapter {guid}'s device names no driver service"
                ));
            }
            service
        }
    };
    if let Some(service) = &nic.service {
        let wdf = format!(r"{SERVICES}\{service}\Parameters\Wdf");
        nic.kmdf_library_version = match reading_of(reg.value(&wdf, "KmdfLibraryVersion")) {
            Ok(reading) => reading,
            Err(code) => {
                errors.push(format!(
                    "the driver service {service}'s KMDF registration could not be read (error \
                     {code})"
                ));
                Reading::Unread
            }
        };
    }
    nic
}

/// `manager.json` as read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DvsFile {
    /// No such file: DVS is not installed.
    Missing,
    Text(String),
    /// It could not be read (why).
    Failed(String),
}

/// Reads `manager.json` (at most [`MAX_DVS_BYTES`]).
pub fn read_dvs_file(path: &Path) -> DvsFile {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return DvsFile::Missing,
        Err(e) => return DvsFile::Failed(e.to_string()),
    };
    let mut text = String::new();
    match file.take(MAX_DVS_BYTES + 1).read_to_string(&mut text) {
        Err(e) => DvsFile::Failed(e.to_string()),
        Ok(n) if (n as u64) > MAX_DVS_BYTES => {
            DvsFile::Failed(format!("it is larger than {MAX_DVS_BYTES} bytes"))
        }
        Ok(_) => DvsFile::Text(text),
    }
}

/// The two fields of `manager.json` the profile reads, typed (an untrusted
/// file never goes through `serde_json::Value`; every other field is
/// skipped).
#[derive(Deserialize)]
struct DvsManager {
    #[serde(rename = "PrimaryInterfaceAddress", default)]
    address: Option<String>,
    #[serde(rename = "PrimaryInterfaceName", default)]
    name: Option<String>,
}

/// What `manager.json` says about DVS's adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DvsConfig {
    /// `PrimaryInterfaceAddress` as written.
    pub address: Option<String>,
    /// `PrimaryInterfaceName` (shown only).
    pub name: Option<String>,
}

/// `manager.json`'s adapter fields (a leading BOM is skipped); why not, by
/// the parser's category and place only.
pub fn dvs_config(text: &str) -> Result<DvsConfig, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let manager: DvsManager = serde_json::from_str(text).map_err(|e| {
        format!(
            "{:?} error at line {} column {}",
            e.classify(),
            e.line(),
            e.column()
        )
    })?;
    Ok(DvsConfig {
        address: manager.address,
        name: manager
            .name
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty()),
    })
}

/// A MAC as lowercase hex with no separators (`6c1ff773c7d9`): `:`, `-` and
/// `.` are dropped; anything else, or not 12 digits, is no MAC.
pub fn normalize_mac(text: &str) -> Option<String> {
    let mut hex = String::new();
    for c in text.trim().chars() {
        match c {
            ':' | '-' | '.' => {}
            c if c.is_ascii_hexdigit() => hex.push(c.to_ascii_lowercase()),
            _ => return None,
        }
    }
    (hex.len() == 12).then_some(hex)
}

/// One network interface (`GetIfTable2`'s `MIB_IF_ROW2`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IfRow {
    /// `InterfaceGuid`, braced and upper case (as `NetCfgInstanceId`).
    pub guid: String,
    /// `PhysicalAddress` ([`mac_hex`]).
    pub mac: String,
    /// `PermanentPhysicalAddress`.
    pub permanent_mac: String,
    /// A hardware interface (not a virtual or software one).
    pub hardware: bool,
    /// A filter module's interface (it repeats its adapter's MAC).
    pub filter: bool,
}

/// `MIB_IF_ROW2.InterfaceAndOperStatusFlags`'s `HardwareInterface` bit.
pub const IF_HARDWARE: u8 = 0x01;
/// Its `FilterInterface` bit.
pub const IF_FILTER: u8 = 0x02;

/// The interface's hardware and filter flags.
pub fn if_flags(bits: u8) -> (bool, bool) {
    ((bits & IF_HARDWARE) != 0, (bits & IF_FILTER) != 0)
}

/// An address's bytes as lowercase hex.
pub fn mac_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A GUID's parts as `8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c`.
pub fn guid_text(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> String {
    let tail: String = d4[2..].iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{d1:08x}-{d2:04x}-{d3:04x}-{:02x}{:02x}-{tail}",
        d4[0], d4[1]
    )
}

/// The hardware adapter with this MAC: filter modules' and virtual
/// interfaces' rows are skipped; the current address wins over the
/// permanent one; none, or several, says why.
pub fn adapter_for_mac<'a>(rows: &'a [IfRow], mac: &str) -> Result<&'a IfRow, String> {
    let candidates: Vec<&IfRow> = rows
        .iter()
        .filter(|r| r.hardware && !r.filter && (r.mac == mac || r.permanent_mac == mac))
        .collect();
    let current: Vec<&IfRow> = candidates
        .iter()
        .copied()
        .filter(|r| r.mac == mac)
        .collect();
    match (candidates.as_slice(), current.as_slice()) {
        ([one], _) | (_, [one]) => Ok(*one),
        ([], _) => Err(format!("no network adapter has the MAC {mac}")),
        _ => Err(format!(
            "{} network adapters have the MAC {mac}",
            candidates.len()
        )),
    }
}

/// Dante Virtual Soundcard and the adapter its `manager.json` names (report
/// only): not installed when there is no file.
pub fn dante(
    reg: &dyn RegistryRead,
    dvs: &DvsFile,
    adapters: &Result<Vec<IfRow>, u32>,
    errors: &mut Vec<String>,
) -> DanteFacts {
    let text = match dvs {
        DvsFile::Missing => {
            return DanteFacts {
                installed: Some(false),
                ..DanteFacts::default()
            };
        }
        DvsFile::Failed(why) => {
            errors.push(format!(
                "Dante Virtual Soundcard's manager.json could not be read: {why}"
            ));
            return DanteFacts::default();
        }
        DvsFile::Text(text) => text,
    };
    let mut dante = DanteFacts {
        installed: Some(true),
        ..DanteFacts::default()
    };
    let config = match dvs_config(text) {
        Ok(config) => config,
        Err(why) => {
            errors.push(format!(
                "Dante Virtual Soundcard's manager.json does not read: {why}"
            ));
            return dante;
        }
    };
    dante.interface_name = config.name;
    let Some(written) = config.address else {
        errors.push(
            "Dante Virtual Soundcard's manager.json names no PrimaryInterfaceAddress".to_string(),
        );
        return dante;
    };
    let Some(mac) = normalize_mac(&written) else {
        errors.push(format!(
            "Dante Virtual Soundcard's PrimaryInterfaceAddress {written:?} is no MAC"
        ));
        dante.interface_address = Some(written);
        return dante;
    };
    dante.interface_address = Some(mac.clone());
    let rows = match adapters {
        Ok(rows) => rows,
        Err(code) => {
            errors.push(format!(
                "the network interfaces could not be listed (error {code})"
            ));
            return dante;
        }
    };
    match adapter_for_mac(rows, &mac) {
        Ok(row) => dante.nic = Some(nic(reg, &row.guid, errors)),
        Err(why) => errors.push(format!("Dante Virtual Soundcard's adapter: {why}")),
    }
    dante
}

/// A policy DWORD that is 1 (`None`: could not be read). A missing value,
/// any other number or a value of another type is not the policy.
pub fn policy_on(value: RegValue, what: &str, errors: &mut Vec<String>) -> Option<bool> {
    match value {
        RegValue::Dword(n) => Some(n == 1),
        RegValue::Failed(code) => {
            errors.push(format!("{what} could not be read (error {code})"));
            None
        }
        RegValue::Absent | RegValue::Text(_) | RegValue::Multi(_) | RegValue::Other(_) => {
            Some(false)
        }
    }
}

/// A DWORD reading (a value of another type cannot be used: unread).
fn dword_reading(value: RegValue, what: &str, errors: &mut Vec<String>) -> Reading<u32> {
    match value {
        RegValue::Dword(n) => Reading::Value(n),
        RegValue::Absent => Reading::Absent,
        RegValue::Failed(code) => {
            errors.push(format!("{what} could not be read (error {code})"));
            Reading::Unread
        }
        RegValue::Text(_) | RegValue::Multi(_) | RegValue::Other(_) => {
            errors.push(format!("{what} is no DWORD"));
            Reading::Unread
        }
    }
}

/// A string reading (a value of another type cannot be used: unread).
fn text_reading(value: RegValue, what: &str, errors: &mut Vec<String>) -> Reading<String> {
    match value {
        RegValue::Text(s) => Reading::Value(s),
        RegValue::Absent => Reading::Absent,
        RegValue::Failed(code) => {
            errors.push(format!("{what} could not be read (error {code})"));
            Reading::Unread
        }
        RegValue::Dword(_) | RegValue::Multi(_) | RegValue::Other(_) => {
            errors.push(format!("{what} is no string"));
            Reading::Unread
        }
    }
}

/// MMCSS's system profile and Pro Audio task.
pub fn mmcss(reg: &dyn RegistryRead, errors: &mut Vec<String>) -> MmcssFacts {
    MmcssFacts {
        system_responsiveness: dword_reading(
            reg.value(MMCSS_PROFILE, "SystemResponsiveness"),
            "MMCSS's SystemResponsiveness",
            errors,
        ),
        network_throttling_index: dword_reading(
            reg.value(MMCSS_PROFILE, "NetworkThrottlingIndex"),
            "MMCSS's NetworkThrottlingIndex",
            errors,
        ),
        pro_audio_priority: dword_reading(
            reg.value(MMCSS_PRO_AUDIO, "Priority"),
            "MMCSS's Pro Audio Priority",
            errors,
        ),
        pro_audio_scheduling: text_reading(
            reg.value(MMCSS_PRO_AUDIO, "Scheduling Category"),
            "MMCSS's Pro Audio Scheduling Category",
            errors,
        ),
    }
}

/// The power API's reads (`PowerGetActiveScheme`, `PowerReadFriendlyName`,
/// `PowerReadACValueIndex`: the documented calls `powercfg` makes, with no
/// process): the scheme's GUID or the error, its name, and its
/// core-parking minimum (`Ok(None)`: the scheme has no such setting).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PowerRead {
    pub scheme: Result<String, u32>,
    pub name: Option<String>,
    pub min_cores: Result<Option<u32>, u32>,
}

/// The power facts, and why one could not be read.
pub fn power_facts(read: &PowerRead, errors: &mut Vec<String>) -> PowerFacts {
    let scheme_guid = match &read.scheme {
        Ok(guid) => Some(guid.clone()),
        Err(code) => {
            errors.push(format!(
                "the active power scheme could not be read (error {code})"
            ));
            None
        }
    };
    let core_parking_min_cores = match read.min_cores {
        Ok(min) => min,
        Err(code) => {
            errors.push(format!(
                "the core-parking minimum could not be read (error {code})"
            ));
            None
        }
    };
    PowerFacts {
        scheme_guid,
        scheme_name: read
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string),
        core_parking_min_cores,
    }
}

/// Every audio-profile fact of one check: the registry through `reg`, the
/// network interfaces, DVS's configuration and the power API as read.
pub fn read_audio(
    reg: &dyn RegistryRead,
    adapters: &Result<Vec<IfRow>, u32>,
    dvs: &DvsFile,
    power: &PowerRead,
) -> AudioFacts {
    let mut errors = Vec::new();
    let (soundgrid_installed, soundgrid_nics) = soundgrid(reg, &mut errors);
    let dante = dante(reg, dvs, adapters, &mut errors);
    let no_auto_update = policy_on(
        reg.value(WINDOWS_UPDATE_AU, "NoAutoUpdate"),
        "Windows Update's NoAutoUpdate",
        &mut errors,
    );
    // Reported only in lane 1: SNV lacks it today. #255 lane 2's
    // `scripts/setup-prod-pc.ps1` applies it on every box; then it gates.
    let drivers_excluded = policy_on(
        reg.value(WINDOWS_UPDATE, "ExcludeWUDriversInQualityUpdate"),
        "Windows Update's ExcludeWUDriversInQualityUpdate",
        &mut errors,
    );
    let mmcss = mmcss(reg, &mut errors);
    let power = power_facts(power, &mut errors);
    AudioFacts {
        soundgrid_installed,
        soundgrid_nics,
        dante,
        no_auto_update,
        drivers_excluded,
        mmcss,
        power,
        errors,
    }
}

#[cfg(test)]
#[path = "prod_env_audio_read_fake.rs"]
mod fake;

#[cfg(test)]
#[path = "prod_env_audio_read_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "prod_env_audio_read_tests_dvs.rs"]
mod tests_dvs;
