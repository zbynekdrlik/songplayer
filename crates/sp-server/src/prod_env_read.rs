//! #233: the raw reads of the production environment and the facts made of
//! them (pure, Linux-tested; `prod_env_win.rs` only reads).
//!
//! The registry is read directly ([`DirectRead`]): it is fast and never
//! hangs. WMI is read behind the bounded reader ([`WmiRead`],
//! `super::Reader`): a WMI hang makes only the WMI facts unreadable, never
//! the registry's.

use sp_core::prod_env::FirewallProfile;

use super::{EnvFacts, WBEM_E_INVALID_CLASS, WBEM_E_INVALID_NAMESPACE};

/// A registry string as read: the value, `None` when the key or the value
/// does not exist, or the Win32 error.
pub type RegString = Result<Option<String>, u32>;

/// A registry DWORD as read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegDword {
    Value(u32),
    /// The key or the value does not exist.
    Absent,
    /// The read failed with this Win32 error.
    Failed(u32),
}

/// A service's state as the service manager reports it: its current state
/// (`SERVICE_STOPPED` = 1, …), or the Win32 error of the read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceRead {
    State(u32),
    Failed(u32),
}

/// What the registry and the service manager give, read directly at each
/// check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectRead {
    /// `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion` `EditionID`.
    pub edition_id: RegString,
    /// The same key's `ProductName` (shown only).
    pub product_name: RegString,
    /// The same key's `CurrentBuildNumber` (`"22631"`).
    pub build: RegString,
    /// Each profile's group-policy `EnableFirewall` (domain, private, public).
    pub firewall_policy: [RegDword; 3],
    /// The older Administrative-Templates policy
    /// (`…\WindowsFirewall\StandardProfile\EnableFirewall`): the private and
    /// public profiles.
    pub firewall_legacy_policy: RegDword,
    /// Each profile's local `EnableFirewall` (domain, private, public).
    pub firewall_local: [RegDword; 3],
    /// The `WinDefend` service.
    pub defender_service: ServiceRead,
}

/// Defender's status row (`MSFT_MpComputerStatus`), each property `None`
/// without a value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DefenderRow {
    /// `RealTimeProtectionEnabled`.
    pub realtime: Option<bool>,
    /// `AMRunningMode`: `Normal`, `Passive Mode`, `EDR Block Mode`, …
    pub mode: Option<String>,
    /// `AntivirusEnabled`.
    pub antivirus_enabled: Option<bool>,
}

/// One firewall profile in the active policy store (`MSFT_NetFirewallProfile`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FirewallRow {
    /// `Domain`, `Private` or `Public`.
    pub name: String,
    /// `Enabled`: 0 off, 1 on, 2 not configured.
    pub enabled: Option<u32>,
}

/// One antivirus product the Security Center lists (`AntiVirusProduct`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvProduct {
    /// `displayName`.
    pub name: String,
    /// `instanceGuid`.
    pub guid: String,
    /// `productState`.
    pub state: Option<u32>,
}

/// What WMI gives, read behind the bound; each query's rows, or its HRESULT.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WmiRead {
    /// Defender's status row (`None`: no instance).
    pub defender: Result<Option<DefenderRow>, i32>,
    /// The firewall profiles as they apply now.
    pub firewall: Result<Vec<FirewallRow>, i32>,
    /// The registered antivirus products.
    pub antivirus: Result<Vec<AvProduct>, i32>,
}

impl WmiRead {
    /// Every query failed with `code` (COM could not start, say).
    pub fn failed(code: i32) -> Self {
        Self {
            defender: Err(code),
            firewall: Err(code),
            antivirus: Err(code),
        }
    }
}

/// Whether a firewall profile is on: the group-policy value wins when it is
/// set (`HKLM\SOFTWARE\Policies\Microsoft\WindowsFirewall\<profile>`), else
/// the local one (`…\SharedAccess\Parameters\FirewallPolicy\<profile>`). Any
/// value but 0 is on. A policy that cannot be read, or a local value that is
/// missing or cannot be read, cannot be verified (`Err`, why).
pub fn firewall_on(
    profile: FirewallProfile,
    policy: RegDword,
    local: RegDword,
) -> Result<bool, String> {
    let name = profile.name();
    match (policy, local) {
        (RegDword::Value(v), _) => Ok(v != 0),
        (RegDword::Failed(code), _) => Err(format!(
            "the {name} firewall policy could not be read (error {code})"
        )),
        (RegDword::Absent, RegDword::Value(v)) => Ok(v != 0),
        (RegDword::Absent, RegDword::Absent) => {
            Err(format!("the {name} firewall setting is missing"))
        }
        (RegDword::Absent, RegDword::Failed(code)) => Err(format!(
            "the {name} firewall setting could not be read (error {code})"
        )),
    }
}

/// Defender's real-time protection from its WMI read: the property
/// (`Ok(Some)`), no instance or no value (`Ok(None)`: cannot be verified), or
/// the query's HRESULT. A missing namespace or class means Defender is not
/// installed: off.
pub fn defender_realtime(read: Result<Option<bool>, i32>) -> Result<bool, String> {
    match read {
        Ok(Some(on)) => Ok(on),
        Ok(None) => Err("Defender's status names no real-time protection".to_string()),
        Err(WBEM_E_INVALID_NAMESPACE | WBEM_E_INVALID_CLASS) => Ok(false),
        Err(code) => Err(format!(
            "Defender's status could not be read (0x{:08X})",
            code as u32
        )),
    }
}

/// Adds `why` to the read errors once (one WMI failure can explain several
/// facts).
fn note(errors: &mut Vec<String>, why: String) {
    if !errors.contains(&why) {
        errors.push(why);
    }
}

/// A registry string fact: the value, or `None` and why.
fn string_fact(read: &RegString, what: &str, errors: &mut Vec<String>) -> Option<String> {
    match read {
        Ok(Some(value)) => Some(value.clone()),
        Ok(None) => {
            note(errors, format!("{what} is missing"));
            None
        }
        Err(code) => {
            note(errors, format!("{what} could not be read (error {code})"));
            None
        }
    }
}

/// The facts of one check: the registry as read now, WMI as read within
/// the bound or why not (`wmi`'s `Err`).
pub fn facts_from(direct: &DirectRead, wmi: &Result<WmiRead, String>) -> EnvFacts {
    let mut errors = Vec::new();
    let edition_id = string_fact(&direct.edition_id, "EditionID", &mut errors);
    let product_name = string_fact(&direct.product_name, "ProductName", &mut errors);
    let mut firewall = [None; 3];
    for (i, profile) in FirewallProfile::ALL.into_iter().enumerate() {
        firewall[i] =
            match firewall_on(profile, direct.firewall_policy[i], direct.firewall_local[i]) {
                Ok(on) => Some(on),
                Err(why) => {
                    note(&mut errors, why);
                    None
                }
            };
    }
    let defender = match wmi {
        Ok(read) => defender_realtime(match &read.defender {
            Ok(row) => Ok(row.as_ref().and_then(|row| row.realtime)),
            Err(code) => Err(*code),
        }),
        Err(why) => Err(why.clone()),
    };
    let defender_realtime = match defender {
        Ok(on) => Some(on),
        Err(why) => {
            note(&mut errors, why);
            None
        }
    };
    EnvFacts {
        edition_id,
        product_name,
        defender_realtime,
        firewall,
        read_errors: errors,
    }
}

#[cfg(test)]
#[path = "prod_env_read_tests.rs"]
mod tests;
