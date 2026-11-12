//! #255 (dstack#155, the owner's ruling of 10.11.2026): the firewall's
//! verdict. Two states are accepted (`sp_core::prod_env::firewall`):
//!
//! - every profile off (a PC with no Dante leg);
//! - dstack's standard: every profile on, its default actions Allow, every
//!   enabled Block rule in the group `dstack155`, which holds exactly 16.
//!
//! Which one the box means: the standard is "attempted" once the group holds
//! a rule, a profile that is on allows by default, or the rules could not be
//! listed (then the box is judged in full, so nothing unread passes). A box
//! with a profile on and none of that is simply a firewall that is on:
//! `firewall_on:<profile>` for each, nothing else. An attempted standard is
//! judged fact by fact, every profile in order (`firewall_off` for one that
//! is off, the default actions of one that is on), then the rules.

use sp_core::prod_env::firewall::{DSTACK_GROUP, DSTACK_GROUP_RULES, FirewallMode};
use sp_core::prod_env::{EnvProblem, FirewallProfile, Unreadable};

use super::EnvFacts;

/// The enabled Block rules of the active store, by group.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockRules {
    /// How many belong to the group `dstack155`.
    pub in_group: u32,
    /// The names of those outside it.
    pub outside: Vec<String>,
}

/// Whether a rule's group (its `RuleGroup` or its `DisplayGroup`) is
/// dstack's (padding and ASCII case ignored).
pub fn in_dstack_group(rule_group: Option<&str>, display_group: Option<&str>) -> bool {
    [rule_group, display_group]
        .into_iter()
        .flatten()
        .any(|g| g.trim().eq_ignore_ascii_case(DSTACK_GROUP))
}

/// Every firewall problem, in order: each profile that cannot be read
/// (nothing else is judged then), else, with a profile on, the state the box
/// is in (module doc).
pub fn problems(facts: &EnvFacts) -> Vec<EnvProblem> {
    let mut problems = Vec::new();
    for (profile, on) in FirewallProfile::ALL.into_iter().zip(facts.firewall) {
        if on.is_none() {
            problems.push(EnvProblem::Unreadable(Unreadable::Firewall(profile)));
        }
    }
    if !problems.is_empty() || !facts.firewall.contains(&Some(true)) {
        return problems;
    }
    let profiles = FirewallProfile::ALL.into_iter().enumerate();
    let defaults_allow = profiles.clone().any(|(i, _)| {
        facts.firewall[i] == Some(true) && facts.firewall_defaults[i].is_some_and(|d| d.allow())
    });
    let attempted = match &facts.firewall_rules {
        Some(rules) => rules.in_group > 0 || defaults_allow,
        None => true,
    };
    for (i, profile) in profiles {
        match (facts.firewall[i], attempted) {
            (Some(true), false) => problems.push(EnvProblem::FirewallOn(profile)),
            (Some(true), true) => match facts.firewall_defaults[i] {
                None => problems.push(EnvProblem::Unreadable(Unreadable::FirewallDefaults(
                    profile,
                ))),
                Some(d) if !d.allow() => problems.push(EnvProblem::FirewallDefaultBlock {
                    profile,
                    defaults: d,
                }),
                Some(_) => {}
            },
            (_, true) => problems.push(EnvProblem::FirewallOff(profile)),
            (_, false) => {}
        }
    }
    if !attempted {
        return problems;
    }
    match &facts.firewall_rules {
        None => problems.push(EnvProblem::Unreadable(Unreadable::FirewallRules)),
        Some(rules) => {
            if !rules.outside.is_empty() {
                problems.push(EnvProblem::FirewallBlockOutsideGroup(rules.outside.clone()));
            }
            if rules.in_group != DSTACK_GROUP_RULES {
                problems.push(EnvProblem::FirewallGroupIncomplete {
                    rules: rules.in_group,
                });
            }
        }
    }
    problems
}

/// The accepted state the firewall is in, given its problems: `None` while
/// a profile is unread, `Off` with every profile off, `Dstack155` with every
/// profile on and no firewall problem, else `Other`.
pub fn mode(facts: &EnvFacts, problems: &[EnvProblem]) -> Option<FirewallMode> {
    if facts.firewall.contains(&None) {
        return None;
    }
    if facts.firewall == [Some(false); 3] {
        return Some(FirewallMode::Off);
    }
    if facts.firewall == [Some(true); 3] && problems.is_empty() {
        return Some(FirewallMode::Dstack155);
    }
    Some(FirewallMode::Other)
}

#[cfg(test)]
#[path = "prod_env_firewall_tests.rs"]
mod tests;
