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

/// "Allow the computer to turn off this device": must carry
/// [`NDIS_DEVICE_DISABLE_PM`]; no value means 0.
pub const PNP_CAPABILITIES: &str = "PnPCapabilities";

/// One value as read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WiredValue {
    pub keyword: &'static str,
    pub value: Reading<String>,
}

/// One wired adapter as read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WiredNicFacts {
    pub guid: String,
    pub name: Option<String>,
    pub description: Option<String>,
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
        Reading::Absent => return Verdict::Missing,
        Reading::Value(text) => text.trim(),
    };
    let holds = text.parse::<u32>().is_ok_and(|n| {
        if value.keyword == PNP_CAPABILITIES {
            (n & NDIS_DEVICE_DISABLE_PM) != 0
        } else {
            n == 0
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
    row.present && row.enabled && row.hardware && !row.filter && row.medium == MEDIUM_802_3
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
    nic.values = ALWAYS
        .iter()
        .chain(WHERE_PRESENT.iter())
        .chain([&PNP_CAPABILITIES])
        .map(|&keyword| WiredValue {
            keyword,
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
/// then each value in the order read.
pub fn problems(nics: &Option<Vec<WiredNicFacts>>) -> Vec<EnvProblem> {
    let Some(nics) = nics else {
        return vec![EnvProblem::Unreadable(Unreadable::WiredNics)];
    };
    let mut problems = Vec::new();
    for nic in nics {
        let name = nic.label();
        let verdicts: Vec<(&str, Verdict)> =
            nic.values.iter().map(|v| (v.keyword, verdict(v))).collect();
        if nic.values.is_empty() || verdicts.iter().any(|(_, v)| *v == Verdict::Unread) {
            problems.push(EnvProblem::WiredNicUnreadable { nic: name.clone() });
        }
        for (keyword, verdict) in verdicts {
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

/// One adapter on the status. `ok` is `None` while anything is unread.
pub fn view(nic: &WiredNicFacts) -> WiredNicView {
    let verdicts: Vec<Verdict> = nic.values.iter().map(verdict).collect();
    let ok = if nic.values.is_empty() || verdicts.contains(&Verdict::Unread) {
        None
    } else {
        Some(verdicts.iter().all(|v| *v == Verdict::Holds))
    };
    WiredNicView {
        guid: nic.guid.clone(),
        name: nic.name.clone(),
        description: nic.description.clone(),
        values: nic
            .values
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
    };
    WiredNicFacts {
        guid: guid.to_string(),
        name: Some(name.to_string()),
        description: None,
        values: ALWAYS
            .iter()
            .chain(WHERE_PRESENT.iter())
            .map(|&k| zero(k))
            .chain([WiredValue {
                keyword: PNP_CAPABILITIES,
                value: Reading::Value("24".to_string()),
            }])
            .collect(),
    }
}

#[cfg(test)]
#[path = "prod_env_audio_wired_tests.rs"]
mod tests;
