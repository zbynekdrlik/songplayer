//! #255 (dstack#155, the owner's ruling of 10.11.2026): the firewall rule
//! follows dstack's two-network standard (dstack `docs/two-networks.md` §3).
//! A production PC is in one of two states:
//!
//! - **off** (a PC with no Dante leg, PP today): every profile off;
//! - **dstack155** (the standard): every profile on, each default action
//!   (inbound and outbound) Allow, every enabled Block rule in the group
//!   [`DSTACK_GROUP`], which holds exactly [`DSTACK_GROUP_RULES`] of them.
//!
//! Anything else is a problem. This module names the facts and their Slovak
//! words; sp-server's `prod_env::firewall` decides.

use serde::{Deserialize, Serialize};

/// The rule group dstack creates (dstack#155).
pub const DSTACK_GROUP: &str = "dstack155";

/// How many enabled Block rules the group holds: in + out for obs64.exe,
/// Arena.exe, python.exe, RustDesk.exe, svchost.exe, mDNSResponder.exe, TCP,
/// and Windows' own mDNS (the Dnscache service, UDP 5353; dstack added it
/// for #309: SongPlayer's `_ndi._tcp` record left on the Dante leg).
pub const DSTACK_GROUP_RULES: u32 = 16;

/// A default action's value (`MSFT_NetFirewallProfile`
/// `DefaultInboundAction` / `DefaultOutboundAction`): Allow.
pub const ACTION_ALLOW: u32 = 2;

/// A default action's value: Block.
pub const ACTION_BLOCK: u32 = 4;

/// A default action's value: not configured.
pub const ACTION_NOT_CONFIGURED: u32 = 0;

/// A profile's default actions as the active store reads them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirewallDefaults {
    pub inbound: u32,
    pub outbound: u32,
}

impl FirewallDefaults {
    /// Both default actions Allow (the standard's item 1).
    pub fn allow(self) -> bool {
        self.inbound == ACTION_ALLOW && self.outbound == ACTION_ALLOW
    }
}

/// Which of the two accepted states the firewall is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FirewallMode {
    /// Every profile off.
    Off,
    /// dstack's standard, every fact as it wants.
    Dstack155,
    /// Neither: the problems say why.
    Other,
}

/// A default action in Slovak.
pub fn action_sk(action: u32) -> String {
    match action {
        ACTION_ALLOW => "povoliť".to_string(),
        ACTION_BLOCK => "blokovať".to_string(),
        ACTION_NOT_CONFIGURED => "nenastavené".to_string(),
        other => format!("hodnota {other}"),
    }
}

/// How many rule names a problem text lists before it counts the rest.
pub const NAMES_SHOWN: usize = 5;

/// Rule names for a problem text: the first [`NAMES_SHOWN`], quoted, then
/// `a ďalšie N`.
pub fn names_sk(names: &[String]) -> String {
    let shown: Vec<String> = names
        .iter()
        .take(NAMES_SHOWN)
        .map(|n| format!("„{n}“"))
        .collect();
    let rest = names.len().saturating_sub(NAMES_SHOWN);
    if rest == 0 {
        shown.join(", ")
    } else {
        format!("{} a ďalšie {rest}", shown.join(", "))
    }
}
