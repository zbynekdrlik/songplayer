//! #255 (the production PC audio profile, lane 1): the audio profile on the
//! environment status — the network adapters the audio transports run on,
//! Windows Update, MMCSS and power — and the report-only facts the dashboard
//! shows as notes ([`notes`]).
//!
//! The owner's directive (12.10.2026): what was found and set by hand on PP
//! and SNV is built into SongPlayer, so a reinstalled or new PC is checked by
//! SongPlayer itself, never set up from memory. sp-server's
//! `prod_env::audio` reads the facts and decides; this module only names
//! them (WASM-safe: the dashboard shows them).

use serde::{Deserialize, Serialize};

/// A network adapter's driver model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DriverModel {
    /// An NDIS 6.x miniport.
    Ndis,
    /// A NetAdapterCx client driver: under load its packets wait for a
    /// worker thread at priority 8 (#233, Microsoft's `executioncontext.cpp`).
    NetAdapterCx,
}

impl DriverModel {
    /// The model's name for the operator.
    pub fn name(self) -> &'static str {
        match self {
            Self::Ndis => "NDIS",
            Self::NetAdapterCx => "NetAdapterCx",
        }
    }
}

/// A network adapter setting the production profile wants off (Waves'
/// SoundGrid PC guide).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NicSetting {
    InterruptModeration,
    FlowControl,
    /// Energy Efficient Ethernet.
    Eee,
    GreenEthernet,
    PowerSaving,
    GigaLite,
    /// "Allow the computer to turn off this device to save power".
    PowerManagement,
}

impl NicSetting {
    /// Every setting, in the order the status lists them.
    pub const ALL: [NicSetting; 7] = [
        Self::InterruptModeration,
        Self::FlowControl,
        Self::Eee,
        Self::GreenEthernet,
        Self::PowerSaving,
        Self::GigaLite,
        Self::PowerManagement,
    ];

    /// The setting's name in a problem code (`soundgrid_nic_setting:<name>`)
    /// and on the status.
    pub fn name(self) -> &'static str {
        match self {
            Self::InterruptModeration => "interrupt_moderation",
            Self::FlowControl => "flow_control",
            Self::Eee => "eee",
            Self::GreenEthernet => "green_ethernet",
            Self::PowerSaving => "power_saving",
            Self::GigaLite => "gigalite",
            Self::PowerManagement => "power_management",
        }
    }

    /// The setting in a Slovak problem text (the driver's own English name
    /// where the operator finds it so in the adapter's properties).
    pub fn name_sk(self) -> &'static str {
        match self {
            Self::InterruptModeration => "Interrupt Moderation (moderovanie prerušení)",
            Self::FlowControl => "Flow Control (riadenie toku)",
            Self::Eee => "Energy Efficient Ethernet",
            Self::GreenEthernet => "Green Ethernet",
            Self::PowerSaving => "Power Saving Mode (úsporný režim)",
            Self::GigaLite => "GigaLite",
            Self::PowerManagement => "vypínanie zariadenia na úsporu energie",
        }
    }
}

/// An MMCSS value the production profile sets (#210 part 2, `vban-out.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MmcssSetting {
    SystemResponsiveness,
    NetworkThrottlingIndex,
    ProAudioPriority,
    ProAudioSchedulingCategory,
}

impl MmcssSetting {
    /// Every value, in the order the status lists them.
    pub const ALL: [MmcssSetting; 4] = [
        Self::SystemResponsiveness,
        Self::NetworkThrottlingIndex,
        Self::ProAudioPriority,
        Self::ProAudioSchedulingCategory,
    ];

    /// The value's name in a problem code (`mmcss:<name>`).
    pub fn name(self) -> &'static str {
        match self {
            Self::SystemResponsiveness => "system_responsiveness",
            Self::NetworkThrottlingIndex => "network_throttling_index",
            Self::ProAudioPriority => "pro_audio_priority",
            Self::ProAudioSchedulingCategory => "pro_audio_scheduling_category",
        }
    }

    /// The value in a Slovak problem text (its registry name).
    pub fn label(self) -> &'static str {
        match self {
            Self::SystemResponsiveness => "SystemResponsiveness",
            Self::NetworkThrottlingIndex => "NetworkThrottlingIndex",
            Self::ProAudioPriority => "Priority úlohy Pro Audio",
            Self::ProAudioSchedulingCategory => "Scheduling Category úlohy Pro Audio",
        }
    }

    /// The production value, as a problem text shows it.
    pub fn wanted(self) -> &'static str {
        match self {
            Self::SystemResponsiveness => "0",
            Self::NetworkThrottlingIndex => "0xFFFFFFFF",
            Self::ProAudioPriority => "6",
            Self::ProAudioSchedulingCategory => "High",
        }
    }
}

/// How a network adapter is named for the operator: its connection name
/// (`Ethernet 4`), else its driver's description, else its GUID. The name is
/// shown only; an adapter is always FOUND by the MAC its driver or its
/// software names, never by name.
pub fn nic_label(name: Option<&str>, description: Option<&str>, guid: &str) -> String {
    name.or(description).unwrap_or(guid).to_string()
}

/// A MAC kept as 12 hex digits (`6c2408e59c91`) as an operator reads it:
/// `6c:24:08:e5:9c:91`. Anything else is shown as it is.
pub fn mac_colons(mac: &str) -> String {
    if mac.len() != 12 || !mac.is_ascii() {
        return mac.to_string();
    }
    let pairs: Vec<&str> = (0..6).map(|i| &mac[2 * i..2 * i + 2]).collect();
    pairs.join(":")
}

/// SoundGrid on this box: the adapter is the one whose MAC the SoundGrid
/// driver's own preferences name (`LocalLanPort`), never the protocol's
/// bindings (on PP those list 14 adapters, virtual and long gone ones
/// among them).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SoundGridState {
    /// The SoundGrid Protocol is not installed (no SoundGrid facts).
    NotInstalled,
    /// The service, the driver's preferences or the interfaces could not be
    /// read.
    Unreadable,
    /// Installed, with no port chosen in the driver.
    PortUnset,
    /// The chosen port's MAC names no adapter.
    PortMissing,
    /// The adapter the driver uses was found.
    Found,
}

/// One adapter setting as read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NicSettingView {
    /// [`NicSetting::name`].
    #[serde(default)]
    pub setting: String,
    /// The registry value it was found under (`*InterruptModeration`); none
    /// when the driver has no such value.
    #[serde(default)]
    pub keyword: Option<String>,
    /// The value as read.
    #[serde(default)]
    pub value: Option<String>,
    /// The value is the production one (`None`: it could not be read).
    #[serde(default)]
    pub ok: Option<bool>,
}

/// One network adapter of an audio transport.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NicView {
    /// Its `NetCfgInstanceId` (`{…}`).
    #[serde(default)]
    pub guid: String,
    /// Its connection name (shown only).
    #[serde(default)]
    pub name: Option<String>,
    /// Its driver's description (shown only).
    #[serde(default)]
    pub description: Option<String>,
    /// Its driver service.
    #[serde(default)]
    pub service: Option<String>,
    /// NetAdapterCx when the service registers a `KmdfLibraryVersion`, else
    /// NDIS (`None`: could not be read).
    #[serde(default)]
    pub driver_model: Option<DriverModel>,
    /// The service's `Parameters\Wdf\KmdfLibraryVersion` (the evidence).
    #[serde(default)]
    pub kmdf_library_version: Option<String>,
    /// The adapter's driver key holds `*IfConnectorPresent`, a keyword only
    /// a NetAdapterCx INF writes (Microsoft's "INF files for NetAdapterCx
    /// client drivers"): more evidence, shown only.
    #[serde(default)]
    pub netadaptercx_keywords: Option<bool>,
    /// Every [`NicSetting`], in its order.
    #[serde(default)]
    pub settings: Vec<NicSettingView>,
    /// Every setting is the production one (`None`: one could not be read).
    #[serde(default)]
    pub settings_ok: Option<bool>,
}

impl NicView {
    /// The adapter's name for the operator ([`nic_label`]).
    pub fn label(&self) -> String {
        nic_label(
            self.name.as_deref(),
            self.description.as_deref(),
            &self.guid,
        )
    }
}

/// Dante Virtual Soundcard and its adapter (report only).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DanteView {
    /// DVS's configuration (`manager.json`) exists (`None`: could not be
    /// read).
    #[serde(default)]
    pub installed: Option<bool>,
    /// Its `PrimaryInterfaceAddress` (the adapter's MAC).
    #[serde(default)]
    pub interface_address: Option<String>,
    /// Its `PrimaryInterfaceName` (shown only).
    #[serde(default)]
    pub interface_name: Option<String>,
    /// The adapter with that MAC.
    #[serde(default)]
    pub nic: Option<NicView>,
}

/// Windows Update's policies.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowsUpdateView {
    /// `NoAutoUpdate` is 1: automatic updates are off (`None`: unread).
    #[serde(default)]
    pub no_auto_update: Option<bool>,
    /// `ExcludeWUDriversInQualityUpdate` is 1: no driver comes through
    /// Windows Update (a gate since #255 lane 2: `scripts/setup-prod-pc.ps1`
    /// applies it at every deploy).
    #[serde(default)]
    pub drivers_excluded: Option<bool>,
}

/// MMCSS's system profile (`None`: missing or unread).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MmcssView {
    #[serde(default)]
    pub system_responsiveness: Option<u32>,
    #[serde(default)]
    pub network_throttling_index: Option<u32>,
    #[serde(default)]
    pub pro_audio_priority: Option<u32>,
    #[serde(default)]
    pub pro_audio_scheduling_category: Option<String>,
}

/// The active power scheme (report only).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerView {
    #[serde(default)]
    pub scheme_guid: Option<String>,
    #[serde(default)]
    pub scheme_name: Option<String>,
    /// The processor's core-parking minimum (`CPMINCORES`, AC), % of cores.
    #[serde(default)]
    pub core_parking_min_cores_pct: Option<u32>,
}

/// The production PC's audio profile on the environment status.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioProfile {
    /// SoundGrid on this box (`None`: an older server).
    #[serde(default)]
    pub soundgrid: Option<SoundGridState>,
    /// The port the SoundGrid driver uses: its preferences' `LocalLanPort`,
    /// 12 lowercase hex digits.
    #[serde(default)]
    pub soundgrid_port_mac: Option<String>,
    /// The adapter with that MAC.
    #[serde(default)]
    pub soundgrid_nic: Option<NicView>,
    #[serde(default)]
    pub dante: DanteView,
    #[serde(default)]
    pub windows_update: WindowsUpdateView,
    #[serde(default)]
    pub mmcss: MmcssView,
    #[serde(default)]
    pub power: PowerView,
    /// The report-only facts that are not the production ones ([`notes`]):
    /// shown, never a problem.
    #[serde(default)]
    pub notes: Vec<String>,
}

/// The report-only facts that are not the production ones, in Slovak: the
/// Dante adapter's driver model (its effect is still being measured), the
/// active power scheme and core parking (the setup script applies it; it
/// stays report-only here). None of them is a problem. The Windows Update
/// driver exclusion was one until #255 lane 2 made it a gate.
pub fn notes(p: &AudioProfile) -> Vec<String> {
    let mut notes = Vec::new();
    match (p.dante.installed, &p.dante.nic) {
        (None, _) => notes.push("inštaláciu Dante Virtual Soundcard sa nepodarilo zistiť".to_string()),
        (Some(false), _) => {}
        (Some(true), None) => {
            let mac = p
                .dante
                .interface_address
                .as_deref()
                .map(|m| format!(" (MAC {m})"))
                .unwrap_or_default();
            notes.push(format!(
                "sieťovú kartu Dante Virtual Soundcard{mac} sa nepodarilo určiť"
            ));
        }
        (Some(true), Some(nic)) => match nic.driver_model {
            Some(DriverModel::NetAdapterCx) => notes.push(format!(
                "sieťová karta Dante (DVS) „{}“ beží na ovládači NetAdapterCx (vplyv sa ešte meria)",
                nic.label()
            )),
            Some(DriverModel::Ndis) => {}
            None => notes.push(format!(
                "ovládač sieťovej karty Dante (DVS) „{}“ sa nepodarilo zistiť",
                nic.label()
            )),
        },
    }
    if p.power.scheme_guid.is_none() {
        notes.push("aktívny plán napájania sa nepodarilo zistiť".to_string());
    }
    match p.power.core_parking_min_cores_pct {
        Some(pct) if pct < FULL_CORES_PCT => notes.push(format!(
            "parkovanie jadier je zapnuté (minimum {pct} % jadier)"
        )),
        Some(_) => {}
        None => notes.push("parkovanie jadier sa nepodarilo zistiť".to_string()),
    }
    notes
}

/// Core parking is off when its minimum keeps every core (100 %).
pub const FULL_CORES_PCT: u32 = 100;

#[cfg(test)]
#[path = "prod_env_audio_tests.rs"]
mod tests;
