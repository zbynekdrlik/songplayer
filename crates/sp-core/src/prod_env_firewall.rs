//! #255 (dstack#155, the owner's ruling of 10.11.2026): the firewall rule
//! follows dstack's two-network standard (dstack `docs/two-networks.md` §3).
//! A production PC is in one of three states:
//!
//! - **off** (a PC with no Dante leg, PP today): every profile off;
//! - **dstack155** (the per-box standard): every profile on, each default
//!   action (inbound and outbound) Allow, every enabled Block rule in the
//!   group [`DSTACK_GROUP`], which holds exactly [`DSTACK_GROUP_RULES`];
//! - **dante_leg** (#322, dstack#264: the generic filter that replaces it):
//!   the same profiles and defaults, every enabled Block rule in the group
//!   [`DANTE_LEG_GROUP`], which holds exactly [`DANTE_LEG_GROUP_RULES`].
//!
//! Both groups at once (a switch half done) is a problem, as is anything
//! else. This module names the facts and their Slovak
//! words; sp-server's `prod_env::firewall` decides.

use serde::{Deserialize, Serialize};

/// The rule group dstack creates (dstack#155).
pub const DSTACK_GROUP: &str = "dstack155";

/// How many enabled Block rules the group holds: in + out for obs64.exe,
/// Arena.exe, python.exe, RustDesk.exe, svchost.exe, mDNSResponder.exe, and
/// TCP in + out.
pub const DSTACK_GROUP_RULES: u32 = 14;

/// #322 (dstack#264): the generic Dante-leg group dstack's
/// `tools/windows/dante-leg.ps1` creates, identical on every PC.
pub const DANTE_LEG_GROUP: &str = "dstack Dante leg";

/// The rules of [`DANTE_LEG_GROUP`] by `DisplayName`, as dstack v0.6.123's
/// `dante-leg.ps1` (`Get-DanteLegRules`) creates them, every one an enabled
/// Block rule bound to the Dante adapter alone.
pub const DANTE_LEG_RULE_NAMES: [&str; 10] = [
    "dstack Dante leg: TCP in",
    "dstack Dante leg: TCP out",
    "dstack Dante leg: UDP in",
    "dstack Dante leg: UDP in, ephemeral ports",
    "dstack Dante leg: svchost.exe UDP inbound",
    "dstack Dante leg: svchost.exe UDP outbound",
    "dstack Dante leg: mDNSResponder.exe UDP inbound",
    "dstack Dante leg: mDNSResponder.exe UDP outbound",
    "dstack Dante leg: mDNSResponder.exe (x86) UDP inbound",
    "dstack Dante leg: mDNSResponder.exe (x86) UDP outbound",
];

/// How many enabled Block rules [`DANTE_LEG_GROUP`] holds.
pub const DANTE_LEG_GROUP_RULES: u32 = 10;

/// One of dstack's two rule groups.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FirewallGroup {
    /// The per-box group of dstack#155.
    Dstack155,
    /// The generic group of dstack#264.
    DanteLeg,
}

impl FirewallGroup {
    /// The group's name, as the firewall lists it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Dstack155 => DSTACK_GROUP,
            Self::DanteLeg => DANTE_LEG_GROUP,
        }
    }

    /// How many enabled Block rules the group holds.
    pub fn rules(self) -> u32 {
        match self {
            Self::Dstack155 => DSTACK_GROUP_RULES,
            Self::DanteLeg => DANTE_LEG_GROUP_RULES,
        }
    }

    /// The group a rule belongs to by its `RuleGroup` or `DisplayGroup`
    /// (padding and ASCII case ignored), or none.
    pub fn of(rule_group: Option<&str>, display_group: Option<&str>) -> Option<Self> {
        [Self::Dstack155, Self::DanteLeg].into_iter().find(|g| {
            [rule_group, display_group]
                .into_iter()
                .flatten()
                .any(|name| name.trim().eq_ignore_ascii_case(g.name()))
        })
    }
}

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
    /// dstack's per-box standard, every fact as it wants.
    Dstack155,
    /// #322: dstack's generic Dante-leg filter, every fact as it wants.
    #[serde(rename = "dante_leg")]
    DanteLeg,
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
