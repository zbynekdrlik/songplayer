//! #285 (the owner, 7.11.2026: "to je vseobecne pravidlo vsade kde mame
//! nejaku sietovku"): every wired adapter of the box carries the profile, not
//! only SoundGrid's. `scripts/setup-prod-pc.ps1` writes these values at every
//! deploy (`Get-ProdPcEveryNicValues`); this check catches them gone BETWEEN
//! deploys: a driver's INF deletes them at install.
//!
//! Which adapters: what `GetIfTable2` lists as hardware, no filter module's
//! row, medium 802.3 (USB ones too), present and turned on: a removed or a
//! disabled adapter keeps a row (`OperStatus` 6, `AdminStatus` 2) and is left
//! out, as the script leaves it alone; a radio is another medium.
//!
//! A value in the registry is what the driver was told, never proof that it
//! obeys: a driver may hide a keyword and ignore it (resolume-snv's RTL8153
//! offers none of them), so this gates the setting, not the PHY.

use sp_core::prod_env::EnvProblem;
use sp_core::prod_env::Unreadable;
use sp_core::prod_env::audio::{WiredNicView, WiredValueView, nic_label};

use super::{IfRow, RegistryRead, class_instance, reading_of, text_of};
use crate::prod_env::audio::{NDIS_DEVICE_DISABLE_PM, Reading};

/// `NDIS_PHYSICAL_MEDIUM`'s `NdisPhysicalMedium802_3`.
pub const MEDIUM_802_3: i32 = 14;

/// `IF_TYPE_ETHERNET_CSMACD`: what the script's `MediaType` 802.3 is.
pub const IF_TYPE_ETHERNET: u32 = 6;

/// The values every wired adapter must hold at 0, offered by its driver or
/// not (`Get-ProdPcEveryNicValues`, the same order: a test compares them).
pub const ALWAYS: [&str; 8] = [
    "*FlowControl",
    "*EEE",
    "EEE",
    "EnableGreenEthernet",
    "AdvancedEEE",
    "EnableExtraPowerSaving",
    "GigaLite",
    "*SelectiveSuspend",
];

/// Those at 0 only where the adapter has them
/// (`Get-ProdPcEveryNicValuesWherePresent`).
pub const WHERE_PRESENT: [&str; 4] = [
    "*InterruptModeration",
    "GreenEthernet",
    "PowerSavingMode",
    "*PowerSavingMode",
];

/// A Realtek RTL8156 USB 2.5GbE adapter ("Realtek Gaming USB 2.5GbE Family
/// Controller", resolume-snv's `Ethernet 4`): its device instance starts
/// with this hardware id (`USB\VID_0BDA&PID_8156\<serial>`).
pub const RTL8156_DEVICE: &str = r"USB\VID_0BDA&PID_8156";

/// The RTL8156's own values (#255 / #285, dstack's receiver A/B of
/// 10.11.2026: "Transmit URBs" 3 to 8 and "Transmit Buffers" 18 to 64 took
/// the receivers' reorders from ~9/h to ~3/h and the FOH card's peak from
/// 9.6 to 4.7 ms). The keywords the driver names them by, read live on
/// resolume-snv (`Get-NetAdapterAdvancedProperty`); its driver offers them,
/// so a missing one is a problem (`Get-ProdPcRtl8156Values`, the same pairs:
/// a test compares them).
///
/// #255 (11.11.2026, the owner: Dante is one audio path, a PC without it
/// passes clean; dstack two-networks §6, dstack PR 179): judged only on an
/// adapter that carries a Dante leg ([`judged`]), and a value its driver does
/// not offer passes ([`WiredValue::offered`]). PP has no leg, and its
/// RTL8156's driver 11.4.211.2022 offers neither keyword.
pub const RTL8156_VALUES: [(&str, u32); 2] = [("PendingTransmits", 8), ("TransmitBufferLen", 64)];

/// Whether a keyword is one of the RTL8156's own values.
fn is_rtl8156_value(keyword: &str) -> bool {
    RTL8156_VALUES.iter().any(|(k, _)| *k == keyword)
}

/// Whether an adapter carries a Dante leg: its connection name is one of
/// `carriers` (`network::leg_carriers`; trimmed, ASCII case ignored).
pub fn carries_a_leg(nic: &WiredNicFacts, carriers: &[String]) -> bool {
    nic.name.as_deref().is_some_and(|name| {
        carriers
            .iter()
            .any(|c| c.trim().eq_ignore_ascii_case(name.trim()))
    })
}

/// Whether a value is judged on an adapter: the RTL8156's own values only
/// on one that carries a Dante leg (#255), every other value everywhere.
pub fn judged(nic: &WiredNicFacts, keyword: &str, carriers: &[String]) -> bool {
    !is_rtl8156_value(keyword) || carries_a_leg(nic, carriers)
}

/// Whether the driver offers a keyword, from its `Ndi\Params` subkeys
/// (`None`: not read, taken as offered).
fn offered_by(params: Option<&[String]>, keyword: &str) -> bool {
    params.is_none_or(|names| names.iter().any(|n| n.trim().eq_ignore_ascii_case(keyword)))
}

/// Whether a device instance id is an RTL8156's (ASCII case ignored).
pub fn is_rtl8156(device_instance: &str) -> bool {
    let id = device_instance.trim().to_ascii_uppercase();
    id.strip_prefix(RTL8156_DEVICE)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('\\') || rest.starts_with('&'))
}

/// The value a keyword must hold: an RTL8156 value's own, else 0.
pub fn wanted(keyword: &str) -> u32 {
    RTL8156_VALUES
        .iter()
        .find(|(k, _)| *k == keyword)
        .map_or(0, |&(_, value)| value)
}

/// "Allow the computer to turn off this device": must carry
/// [`NDIS_DEVICE_DISABLE_PM`]; no value means 0.
pub const PNP_CAPABILITIES: &str = "PnPCapabilities";

/// One value as read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WiredValue {
    pub keyword: &'static str,
    pub value: Reading<String>,
    /// #255: the adapter's driver offers it (its class key's `Ndi\Params`
    /// lists it); read for the RTL8156's values only, `true` for the rest
    /// (written to the class key whether offered or not) and when the list
    /// could not be read. A value the driver does not offer and the
    /// registry lacks passes (dstack PR 179).
    pub offered: bool,
}

/// One wired adapter as read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WiredNicFacts {
    pub guid: String,
    pub name: Option<String>,
    pub description: Option<String>,
    /// Its device instance id (the class instance's `DeviceInstanceID`, else
    /// the connection's `PnPInstanceId`); `None`: neither names one, so
    /// whether it is an RTL8156 is not known (unreadable).
    pub device: Option<String>,
    /// Empty: its instance in the network class was not found.
    pub values: Vec<WiredValue>,
}

impl WiredNicFacts {
    /// The adapter's name for the operator.
    pub fn label(&self) -> String {
        nic_label(
            self.name.as_deref(),
            self.description.as_deref(),
            &self.guid,
        )
    }
}

/// One value's verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Holds,
    /// Not the production value: as read.
    Wrong(String),
    /// A value the adapter must have is not there.
    Missing,
    Unread,
}

/// A value's verdict. Absent holds only for [`WHERE_PRESENT`]; a value that
/// is no number is wrong.
pub fn verdict(value: &WiredValue) -> Verdict {
    let text = match &value.value {
        Reading::Unread => return Verdict::Unread,
        Reading::Absent if WHERE_PRESENT.contains(&value.keyword) => return Verdict::Holds,
        Reading::Absent if !value.offered => return Verdict::Holds,
        Reading::Absent => return Verdict::Missing,
        Reading::Value(text) => text.trim(),
    };
    let holds = text.parse::<u32>().is_ok_and(|n| {
        if value.keyword == PNP_CAPABILITIES {
            (n & NDIS_DEVICE_DISABLE_PM) != 0
        } else {
            n == wanted(value.keyword)
        }
    });
    if holds {
        Verdict::Holds
    } else {
        Verdict::Wrong(text.to_string())
    }
}

/// Whether a row is a wired adapter of its own, in use: present and turned
/// on. A removed adapter's row (not present) and a disabled one carry no
/// traffic, and `setup-prod-pc` leaves both alone (`Not Present`,
/// `Disabled`).
pub fn is_wired(row: &IfRow) -> bool {
    row.present && row.enabled && row.hardware && !row.filter && row.if_type == IF_TYPE_ETHERNET
}

/// One adapter's values. What cannot be read stays unread, and says why.
fn wired_nic(reg: &dyn RegistryRead, guid: &str, errors: &mut Vec<String>) -> WiredNicFacts {
    let connection = format!(r"{}\{guid}\Connection", super::NET_CONNECTIONS);
    let mut nic = WiredNicFacts {
        guid: guid.to_string(),
        name: text_of(reg.value(&connection, "Name")),
        ..WiredNicFacts::default()
    };
    let Some(key) = class_instance(reg, guid, errors) else {
        return nic;
    };
    nic.description = text_of(reg.value(&key, "DriverDesc"));
    nic.device = text_of(reg.value(&key, "DeviceInstanceID"))
        .or_else(|| text_of(reg.value(&connection, "PnPInstanceId")));
    if nic.device.is_none() {
        errors.push(format!(
            "the network adapter {guid} names no device instance"
        ));
    }
    let rtl8156 = nic.device.as_deref().is_some_and(is_rtl8156);
    let own: &[(&'static str, u32)] = if rtl8156 { &RTL8156_VALUES } else { &[] };
    let params = rtl8156
        .then(|| reg.subkeys(&format!(r"{key}\Ndi\Params")).ok())
        .flatten();
    nic.values = ALWAYS
        .iter()
        .chain(WHERE_PRESENT.iter())
        .chain([&PNP_CAPABILITIES])
        .chain(own.iter().map(|(keyword, _)| keyword))
        .map(|&keyword| WiredValue {
            keyword,
            offered: !is_rtl8156_value(keyword) || offered_by(params.as_deref(), keyword),
            value: reading_of(reg.value(&key, keyword)).unwrap_or_else(|code| {
                errors.push(format!(
                    "the network adapter {guid}'s {keyword} could not be read (error {code})"
                ));
                Reading::Unread
            }),
        })
        .collect();
    nic
}

/// Every wired adapter, by GUID (a stable order: a change of problems is
/// logged). `None`: the interfaces could not be listed (why: `errors`).
pub fn wired_nics(
    reg: &dyn RegistryRead,
    adapters: &Result<Vec<IfRow>, u32>,
    errors: &mut Vec<String>,
) -> Option<Vec<WiredNicFacts>> {
    let rows = match adapters {
        Ok(rows) => rows,
        Err(code) => {
            errors.push(format!(
                "the wired adapters could not be listed (error {code})"
            ));
            return None;
        }
    };
    let mut guids: Vec<&str> = rows
        .iter()
        .filter(|r| is_wired(r))
        .map(|r| r.guid.as_str())
        .collect();
    guids.sort_unstable();
    guids.dedup();
    Some(
        guids
            .into_iter()
            .map(|guid| wired_nic(reg, guid, errors))
            .collect(),
    )
}

/// The problems of every wired adapter: one "cannot be read" per adapter,
/// then each value in the order read; every adapter taken as carrying a
/// Dante leg (the tests' resolume-snv: each value judged).
#[cfg(test)]
pub fn problems(nics: &Option<Vec<WiredNicFacts>>) -> Vec<EnvProblem> {
    let carriers: Vec<String> = nics
        .iter()
        .flatten()
        .filter_map(|n| n.name.clone())
        .collect();
    problems_besides(nics, &[], &carriers)
}

/// The problems, leaving out each `(guid, keyword)` another check already
/// judges on that adapter (the SoundGrid adapter's own settings), so one
/// wrong value is named once, and each value not [`judged`] there (the
/// RTL8156's off the adapters in `carriers`).
pub fn problems_besides(
    nics: &Option<Vec<WiredNicFacts>>,
    elsewhere: &[(String, String)],
    carriers: &[String],
) -> Vec<EnvProblem> {
    let Some(nics) = nics else {
        return vec![EnvProblem::Unreadable(Unreadable::WiredNics)];
    };
    let mut problems = Vec::new();
    for nic in nics {
        let name = nic.label();
        let verdicts: Vec<(&str, Verdict)> = nic
            .values
            .iter()
            .filter(|v| judged(nic, v.keyword, carriers))
            .map(|v| (v.keyword, verdict(v)))
            .collect();
        if nic.values.is_empty()
            || nic.device.is_none()
            || verdicts.iter().any(|(_, v)| *v == Verdict::Unread)
        {
            problems.push(EnvProblem::WiredNicUnreadable { nic: name.clone() });
        }
        for (keyword, verdict) in verdicts {
            let judged_elsewhere = elsewhere
                .iter()
                .any(|(g, k)| g.eq_ignore_ascii_case(&nic.guid) && k.eq_ignore_ascii_case(keyword));
            if judged_elsewhere {
                continue;
            }
            let value = match verdict {
                Verdict::Wrong(value) => Some(value),
                Verdict::Missing => None,
                Verdict::Holds | Verdict::Unread => continue,
            };
            problems.push(EnvProblem::WiredNicValue {
                nic: name.clone(),
                keyword: keyword.to_string(),
                value,
            });
        }
    }
    problems
}

/// One adapter on the status, every adapter taken as carrying a Dante leg
/// (the tests').
#[cfg(test)]
pub fn view(nic: &WiredNicFacts) -> WiredNicView {
    view_on(nic, &nic.name.clone().into_iter().collect::<Vec<_>>())
}

/// One adapter on the status: the values [`judged`] there (`carriers`).
/// `ok` is `None` while anything is unread.
pub fn view_on(nic: &WiredNicFacts, carriers: &[String]) -> WiredNicView {
    let shown: Vec<&WiredValue> = nic
        .values
        .iter()
        .filter(|v| judged(nic, v.keyword, carriers))
        .collect();
    let verdicts: Vec<Verdict> = shown.iter().map(|v| verdict(v)).collect();
    let ok = if nic.values.is_empty() || nic.device.is_none() || verdicts.contains(&Verdict::Unread)
    {
        None
    } else {
        Some(verdicts.iter().all(|v| *v == Verdict::Holds))
    };
    WiredNicView {
        guid: nic.guid.clone(),
        name: nic.name.clone(),
        description: nic.description.clone(),
        values: shown
            .iter()
            .zip(verdicts)
            .map(|(v, verdict)| WiredValueView {
                keyword: v.keyword.to_string(),
                value: v.value.value(),
                ok: match verdict {
                    Verdict::Holds => Some(true),
                    Verdict::Wrong(_) | Verdict::Missing => Some(false),
                    Verdict::Unread => None,
                },
            })
            .collect(),
        ok,
    }
}

/// A wired adapter whose every value holds (the tests' fixture).
#[cfg(test)]
pub fn example(guid: &str, name: &str) -> WiredNicFacts {
    let zero = |keyword| WiredValue {
        keyword,
        value: Reading::Value("0".to_string()),
        offered: true,
    };
    WiredNicFacts {
        guid: guid.to_string(),
        name: Some(name.to_string()),
        description: None,
        device: Some(r"PCI\VEN_10EC&DEV_8125\0".to_string()),
        values: ALWAYS
            .iter()
            .chain(WHERE_PRESENT.iter())
            .map(|&k| zero(k))
            .chain([WiredValue {
                keyword: PNP_CAPABILITIES,
                value: Reading::Value("24".to_string()),
                offered: true,
            }])
            .collect(),
    }
}

#[cfg(test)]
#[path = "prod_env_audio_wired_tests.rs"]
mod tests;
