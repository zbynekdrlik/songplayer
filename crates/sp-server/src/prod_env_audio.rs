//! #255 (the production PC audio profile, lane 1): the audio profile's
//! facts and their verdict, part of the production-environment check.
//!
//! The owner's directive (12.10.2026): what was found and set by hand on PP
//! and SNV is built into SongPlayer, checked at the start and every minute,
//! shown on the HealthBar and gated at every deploy. What this module
//! decides, every rule on facts read with no process spawned
//! (`prod_env_audio_read.rs` walks the registry through a reader that
//! `prod_env_audio_win.rs` implements on Windows):
//!
//! - **The SoundGrid adapters** (GATE): every adapter the SoundGrid Protocol
//!   is bound to runs an NDIS driver — NetAdapterCx starved SoundGrid on PP
//!   (#233: 4 loop events and +80 resyncs before, 0 after the NDIS driver)
//!   — with Waves' settings off ([`setting_verdict`]).
//! - **Windows Update** (GATE): automatic updates off (`NoAutoUpdate` 1).
//!   The driver exclusion is reported only until lane 2's setup applies it.
//! - **MMCSS** (GATE, #210 part 2): `SystemResponsiveness` 0,
//!   `NetworkThrottlingIndex` 0xFFFFFFFF, the Pro Audio task at priority 6,
//!   scheduling category High.
//! - **Reported only**: Dante Virtual Soundcard's adapter (its effect is
//!   still being measured), the power scheme and core parking
//!   (`sp_core::prod_env::audio::notes`).

use sp_core::prod_env::audio::{
    AudioProfile, DanteView, DriverModel, MmcssSetting, MmcssView, NicSetting, NicSettingView,
    NicView, PowerView, WindowsUpdateView, nic_label, notes,
};
use sp_core::prod_env::{EnvProblem, Unreadable};

/// A registry fact as read: not read (why: the read errors), absent, or its
/// value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reading<T> {
    Unread,
    Absent,
    Value(T),
}

impl<T> Default for Reading<T> {
    fn default() -> Self {
        Self::Unread
    }
}

impl<T: Clone> Reading<T> {
    /// The value, if one was read.
    pub fn value(&self) -> Option<T> {
        match self {
            Self::Value(v) => Some(v.clone()),
            Self::Unread | Self::Absent => None,
        }
    }
}

/// One adapter setting as read: the registry value it was found under (the
/// first of its names that exists) and its value as text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingRead {
    pub setting: NicSetting,
    pub keyword: Option<String>,
    pub value: Reading<String>,
}

/// One network adapter as read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NicFacts {
    /// Its `NetCfgInstanceId`.
    pub guid: String,
    /// Its connection name (shown only).
    pub name: Option<String>,
    /// Its driver's description (shown only).
    pub description: Option<String>,
    /// Its driver service (the device instance's `Service`).
    pub service: Option<String>,
    /// The service's `Parameters\Wdf\KmdfLibraryVersion`.
    pub kmdf_library_version: Reading<String>,
    /// Its driver key holds `*IfConnectorPresent` (evidence, shown only).
    pub netadaptercx_keywords: Option<bool>,
    /// The settings read from its driver key (a setting not listed was not
    /// read).
    pub settings: Vec<SettingRead>,
}

impl NicFacts {
    /// The adapter's name for the operator.
    pub fn label(&self) -> String {
        nic_label(
            self.name.as_deref(),
            self.description.as_deref(),
            &self.guid,
        )
    }

    /// One setting as read; a setting not listed was not read.
    pub fn setting(&self, setting: NicSetting) -> SettingRead {
        self.settings
            .iter()
            .find(|s| s.setting == setting)
            .cloned()
            .unwrap_or(SettingRead {
                setting,
                keyword: None,
                value: Reading::Unread,
            })
    }
}

/// Dante Virtual Soundcard as read (report only).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DanteFacts {
    /// Its `manager.json` exists (`None`: could not be read).
    pub installed: Option<bool>,
    /// `PrimaryInterfaceAddress` (the adapter's MAC).
    pub interface_address: Option<String>,
    /// `PrimaryInterfaceName` (shown only).
    pub interface_name: Option<String>,
    /// The adapter with that MAC.
    pub nic: Option<NicFacts>,
}

/// MMCSS's system profile as read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MmcssFacts {
    pub system_responsiveness: Reading<u32>,
    pub network_throttling_index: Reading<u32>,
    pub pro_audio_priority: Reading<u32>,
    pub pro_audio_scheduling: Reading<String>,
}

/// The active power scheme (report only).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PowerFacts {
    pub scheme_guid: Option<String>,
    pub scheme_name: Option<String>,
    pub core_parking_min_cores: Option<u32>,
}

/// The audio profile's facts. The default is nothing read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioFacts {
    /// The SoundGrid Protocol's service exists.
    pub soundgrid_installed: Option<bool>,
    /// Every adapter it is bound to (`None`: the binding could not be read).
    pub soundgrid_nics: Option<Vec<NicFacts>>,
    pub dante: DanteFacts,
    /// `NoAutoUpdate` is 1.
    pub no_auto_update: Option<bool>,
    /// `ExcludeWUDriversInQualityUpdate` is 1 (report only; lane 2's setup
    /// applies it, then it becomes a gate).
    pub drivers_excluded: Option<bool>,
    pub mmcss: MmcssFacts,
    pub power: PowerFacts,
    /// Why a fact could not be read.
    pub errors: Vec<String>,
}

/// NDIS's `NDIS_DEVICE_DISABLE_PM` bit of `PnPCapabilities` (ndis.h): set =
/// "Allow the computer to turn off this device to save power" is off. The
/// adapter's Power Management tab writes 24 (0x18: this bit and
/// `NDIS_DEVICE_DISABLE_WAKE_UP` 0x10) when that box is cleared; Microsoft's
/// KB 2740020 deploys 24 or 280 (0x118), both with this bit. No value means
/// 0: Windows may turn the adapter off.
pub const NDIS_DEVICE_DISABLE_PM: u32 = 0x08;

/// A setting every adapter must have: the driver of a SoundGrid adapter
/// that cannot turn it off cannot pass (Waves' guide names both).
pub fn required(setting: NicSetting) -> bool {
    matches!(
        setting,
        NicSetting::InterruptModeration | NicSetting::FlowControl
    )
}

/// Whether a setting's value is the production one: `PnPCapabilities` with
/// [`NDIS_DEVICE_DISABLE_PM`] set, every other setting 0 (off).
pub fn setting_holds(setting: NicSetting, value: u32) -> bool {
    match setting {
        NicSetting::PowerManagement => (value & NDIS_DEVICE_DISABLE_PM) != 0,
        _ => value == 0,
    }
}

/// One setting's verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingVerdict {
    Holds,
    /// Not off: the value as the problem names it.
    Wrong(String),
    /// A required setting the driver does not have.
    Missing,
    Unread,
}

/// A setting's verdict: an optional setting the driver does not have holds;
/// a missing `PnPCapabilities` is 0 (the adapter may be turned off); a
/// value that is no number is wrong.
pub fn setting_verdict(read: &SettingRead) -> SettingVerdict {
    let value = match &read.value {
        Reading::Unread => return SettingVerdict::Unread,
        Reading::Absent if read.setting == NicSetting::PowerManagement => "0".to_string(),
        Reading::Absent if required(read.setting) => return SettingVerdict::Missing,
        Reading::Absent => return SettingVerdict::Holds,
        Reading::Value(v) => v.trim().to_string(),
    };
    match value.parse::<u32>() {
        Ok(n) if setting_holds(read.setting, n) => SettingVerdict::Holds,
        _ => SettingVerdict::Wrong(value),
    }
}

/// The adapter's driver model: NetAdapterCx when its service registers a
/// `KmdfLibraryVersion` (a NetAdapterCx client is a KMDF driver bound to the
/// class extension; PP's NDIS `rt640x64` has `Parameters\Wdf` with only the
/// WDF versions, its NetAdapterCx `rtucx21x64` `KmdfLibraryVersion` 1.33),
/// else NDIS. `None`: could not be read.
pub fn driver_model(kmdf: &Reading<String>) -> Option<DriverModel> {
    match kmdf {
        Reading::Unread => None,
        Reading::Absent => Some(DriverModel::Ndis),
        Reading::Value(_) => Some(DriverModel::NetAdapterCx),
    }
}

/// The problems of one SoundGrid adapter, in order: cannot be read, its
/// driver model, then each setting in [`NicSetting::ALL`]'s order.
pub fn nic_problems(nic: &NicFacts) -> Vec<EnvProblem> {
    let name = nic.label();
    let model = driver_model(&nic.kmdf_library_version);
    let verdicts: Vec<(NicSetting, SettingVerdict)> = NicSetting::ALL
        .iter()
        .map(|&s| (s, setting_verdict(&nic.setting(s))))
        .collect();
    let mut problems = Vec::new();
    let unread = verdicts.iter().any(|(_, v)| *v == SettingVerdict::Unread);
    if model.is_none() || unread {
        problems.push(EnvProblem::SoundGridNicUnreadable { nic: name.clone() });
    }
    if model == Some(DriverModel::NetAdapterCx) {
        problems.push(EnvProblem::SoundGridNicNetAdapterCx { nic: name.clone() });
    }
    for (setting, verdict) in verdicts {
        let value = match verdict {
            SettingVerdict::Wrong(value) => Some(value),
            SettingVerdict::Missing => None,
            SettingVerdict::Holds | SettingVerdict::Unread => continue,
        };
        problems.push(EnvProblem::SoundGridNicSetting {
            nic: name.clone(),
            setting,
            value,
        });
    }
    problems
}

/// `SystemResponsiveness` of a production PC: no CPU kept back from the
/// multimedia tasks.
pub const SYSTEM_RESPONSIVENESS: u32 = 0;

/// `NetworkThrottlingIndex` of a production PC: MMCSS's network throttling
/// off.
pub const NETWORK_THROTTLING_OFF: u32 = 0xFFFF_FFFF;

/// The Pro Audio task's `Priority` on a production PC.
pub const PRO_AUDIO_PRIORITY: u32 = 6;

/// The Pro Audio task's `Scheduling Category` on a production PC (ASCII
/// case ignored).
pub const PRO_AUDIO_SCHEDULING: &str = "High";

/// An MMCSS value as a problem shows it: `NetworkThrottlingIndex` in hex,
/// the rest in decimal.
pub fn mmcss_shown(setting: MmcssSetting, value: u32) -> String {
    match setting {
        MmcssSetting::NetworkThrottlingIndex => format!("0x{value:08X}"),
        _ => value.to_string(),
    }
}

/// MMCSS's problems: one "cannot be read" first when any value could not
/// be read, then each value that is missing or not the production one.
pub fn mmcss_problems(m: &MmcssFacts) -> Vec<EnvProblem> {
    let mut unread = false;
    let mut problems = Vec::new();
    let dwords = [
        (
            MmcssSetting::SystemResponsiveness,
            &m.system_responsiveness,
            SYSTEM_RESPONSIVENESS,
        ),
        (
            MmcssSetting::NetworkThrottlingIndex,
            &m.network_throttling_index,
            NETWORK_THROTTLING_OFF,
        ),
        (
            MmcssSetting::ProAudioPriority,
            &m.pro_audio_priority,
            PRO_AUDIO_PRIORITY,
        ),
    ];
    for (setting, read, wanted) in dwords {
        let value = match read {
            Reading::Unread => {
                unread = true;
                continue;
            }
            Reading::Absent => None,
            Reading::Value(v) if *v == wanted => continue,
            Reading::Value(v) => Some(mmcss_shown(setting, *v)),
        };
        problems.push(EnvProblem::Mmcss { setting, value });
    }
    let setting = MmcssSetting::ProAudioSchedulingCategory;
    match &m.pro_audio_scheduling {
        Reading::Unread => unread = true,
        Reading::Absent => problems.push(EnvProblem::Mmcss {
            setting,
            value: None,
        }),
        Reading::Value(v) if v.trim().eq_ignore_ascii_case(PRO_AUDIO_SCHEDULING) => {}
        Reading::Value(v) => problems.push(EnvProblem::Mmcss {
            setting,
            value: Some(v.trim().to_string()),
        }),
    }
    if unread {
        problems.insert(0, EnvProblem::Unreadable(Unreadable::Mmcss));
    }
    problems
}

/// The audio profile's problems, in order: the SoundGrid binding and each
/// adapter, Windows Update, MMCSS. The report-only facts are never one.
pub fn problems(audio: &AudioFacts) -> Vec<EnvProblem> {
    let mut problems = Vec::new();
    match &audio.soundgrid_nics {
        None => problems.push(EnvProblem::Unreadable(Unreadable::SoundGrid)),
        Some(nics) => {
            for nic in nics {
                problems.extend(nic_problems(nic));
            }
        }
    }
    match audio.no_auto_update {
        None => problems.push(EnvProblem::Unreadable(Unreadable::WindowsUpdate)),
        Some(false) => problems.push(EnvProblem::AutoUpdateOn),
        Some(true) => {}
    }
    problems.extend(mmcss_problems(&audio.mmcss));
    problems
}

/// One adapter on the status.
pub fn nic_view(nic: &NicFacts) -> NicView {
    let settings: Vec<NicSettingView> = NicSetting::ALL
        .iter()
        .map(|&setting| {
            let read = nic.setting(setting);
            let ok = match setting_verdict(&read) {
                SettingVerdict::Holds => Some(true),
                SettingVerdict::Wrong(_) | SettingVerdict::Missing => Some(false),
                SettingVerdict::Unread => None,
            };
            NicSettingView {
                setting: setting.name().to_string(),
                keyword: read.keyword,
                value: read.value.value(),
                ok,
            }
        })
        .collect();
    let settings_ok = settings
        .iter()
        .map(|s| s.ok)
        .collect::<Option<Vec<bool>>>()
        .map(|oks| oks.into_iter().all(|ok| ok));
    NicView {
        guid: nic.guid.clone(),
        name: nic.name.clone(),
        description: nic.description.clone(),
        service: nic.service.clone(),
        driver_model: driver_model(&nic.kmdf_library_version),
        kmdf_library_version: nic.kmdf_library_version.value(),
        netadaptercx_keywords: nic.netadaptercx_keywords,
        settings,
        settings_ok,
    }
}

/// The audio profile on the status, with its report-only notes.
pub fn view(audio: &AudioFacts) -> AudioProfile {
    let mut profile = AudioProfile {
        soundgrid_installed: audio.soundgrid_installed,
        soundgrid_nics: audio
            .soundgrid_nics
            .as_ref()
            .map(|nics| nics.iter().map(nic_view).collect()),
        dante: DanteView {
            installed: audio.dante.installed,
            interface_address: audio.dante.interface_address.clone(),
            interface_name: audio.dante.interface_name.clone(),
            nic: audio.dante.nic.as_ref().map(nic_view),
        },
        windows_update: WindowsUpdateView {
            no_auto_update: audio.no_auto_update,
            drivers_excluded: audio.drivers_excluded,
        },
        mmcss: MmcssView {
            system_responsiveness: audio.mmcss.system_responsiveness.value(),
            network_throttling_index: audio.mmcss.network_throttling_index.value(),
            pro_audio_priority: audio.mmcss.pro_audio_priority.value(),
            pro_audio_scheduling_category: audio.mmcss.pro_audio_scheduling.value(),
        },
        power: PowerView {
            scheme_guid: audio.power.scheme_guid.clone(),
            scheme_name: audio.power.scheme_name.clone(),
            core_parking_min_cores_pct: audio.power.core_parking_min_cores,
        },
        notes: Vec::new(),
    };
    profile.notes = notes(&profile);
    profile
}

/// A production PC's audio facts (the tests' fixture): PP's SoundGrid
/// adapter on its NDIS driver with every setting off, DVS on a NetAdapterCx
/// adapter (report only), automatic updates off, the driver exclusion
/// missing (report only), MMCSS set, no core parking.
#[cfg(test)]
pub fn production_example() -> AudioFacts {
    let nic = |guid: &str, name: &str, service: &str, kmdf: Reading<String>| NicFacts {
        guid: guid.to_string(),
        name: Some(name.to_string()),
        description: None,
        service: Some(service.to_string()),
        kmdf_library_version: kmdf,
        netadaptercx_keywords: Some(false),
        settings: NicSetting::ALL
            .iter()
            .map(|&setting| SettingRead {
                setting,
                keyword: None,
                value: if setting == NicSetting::PowerManagement {
                    Reading::Value("24".to_string())
                } else {
                    Reading::Value("0".to_string())
                },
            })
            .collect(),
    };
    AudioFacts {
        soundgrid_installed: Some(true),
        soundgrid_nics: Some(vec![nic(
            "{DBFC2CF4-0000-0000-0000-000000000001}",
            "soundgrid-pcie",
            "rt640x64",
            Reading::Absent,
        )]),
        dante: DanteFacts {
            installed: Some(true),
            interface_address: Some("6c1ff773c7d9".to_string()),
            interface_name: Some("Ethernet 4".to_string()),
            nic: Some(nic(
                "{0B5E0000-0000-0000-0000-000000000002}",
                "Ethernet 4",
                "rtucx21x64",
                Reading::Value("1.33".to_string()),
            )),
        },
        no_auto_update: Some(true),
        drivers_excluded: Some(false),
        mmcss: MmcssFacts {
            system_responsiveness: Reading::Value(SYSTEM_RESPONSIVENESS),
            network_throttling_index: Reading::Value(NETWORK_THROTTLING_OFF),
            pro_audio_priority: Reading::Value(PRO_AUDIO_PRIORITY),
            pro_audio_scheduling: Reading::Value("High".to_string()),
        },
        power: PowerFacts {
            scheme_guid: Some("8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c".to_string()),
            scheme_name: Some("High performance".to_string()),
            core_parking_min_cores: Some(100),
        },
        errors: Vec::new(),
    }
}

#[path = "prod_env_audio_read.rs"]
pub mod read;

#[cfg(windows)]
#[path = "prod_env_audio_win.rs"]
pub(super) mod win;

#[cfg(test)]
#[path = "prod_env_audio_tests.rs"]
mod tests;
