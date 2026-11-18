//! #255 (dstack#155, the owner's ruling of 10.11.2026), #322 (dstack#264):
//! the firewall's verdict. Three states are accepted
//! (`sp_core::prod_env::firewall`):
//!
//! - every profile off (a PC with no Dante leg);
//! - dstack's per-box standard: every profile on, its default actions Allow,
//!   every enabled Block rule in the group `dstack155`, which holds exactly
//!   14;
//! - dstack's generic filter (#322): the same, the group `dstack Dante leg`
//!   with exactly 10. Both groups holding rules is `firewall_groups_mixed`,
//!   so a switch half done never passes.
//!
//! Which one the box means: the standard is "attempted" once either group
//! holds a rule, a profile that is on allows by default, or the rules could
//! not be listed (then the box is judged in full, so nothing unread passes).
//! A box with a profile on and none of that is simply a firewall that is on:
//! `firewall_on:<profile>` for each, nothing else. An attempted standard is
//! judged fact by fact, every profile in order (`firewall_off` for one that
//! is off, the default actions of one that is on), then the rules.

use sp_core::prod_env::firewall::{FirewallGroup, FirewallMode};
use sp_core::prod_env::{EnvProblem, FirewallProfile, Unreadable};

use super::EnvFacts;

/// The enabled Block rules of the active store, by group.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockRules {
    /// How many belong to the group `dstack155`.
    pub dstack155: u32,
    /// #322: how many belong to the group `dstack Dante leg`.
    pub dante_leg: u32,
    /// The names of those outside both.
    pub outside: Vec<String>,
}

impl BlockRules {
    /// Counts one rule of `group` (`None`: outside both, by `name`).
    pub fn add(&mut self, group: Option<FirewallGroup>, name: String) {
        match group {
            Some(FirewallGroup::Dstack155) => self.dstack155 += 1,
            Some(FirewallGroup::DanteLeg) => self.dante_leg += 1,
            None => self.outside.push(name),
        }
    }

    /// The group the rules are judged by: the one that holds rules (the
    /// generic one when neither does), `None` when both do.
    pub fn group(&self) -> Option<FirewallGroup> {
        match (self.dstack155 > 0, self.dante_leg > 0) {
            (true, true) => None,
            (true, false) => Some(FirewallGroup::Dstack155),
            (false, _) => Some(FirewallGroup::DanteLeg),
        }
    }

    /// How many rules `group` holds.
    fn count(&self, group: FirewallGroup) -> u32 {
        match group {
            FirewallGroup::Dstack155 => self.dstack155,
            FirewallGroup::DanteLeg => self.dante_leg,
        }
    }
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
        Some(rules) => rules.dstack155 > 0 || rules.dante_leg > 0 || defaults_allow,
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
            match rules.group() {
                None => problems.push(EnvProblem::FirewallGroupsMixed {
                    dstack155: rules.dstack155,
                    dante_leg: rules.dante_leg,
                }),
                Some(group) if rules.count(group) != group.rules() => {
                    problems.push(EnvProblem::FirewallGroupIncomplete {
                        group,
                        rules: rules.count(group),
                    })
                }
                Some(_) => {}
            }
        }
    }
    problems
}

/// The accepted state the firewall is in, given its problems: `None` while
/// a profile is unread, `Off` with every profile off, `Dstack155` /
/// `DanteLeg` with every profile on, no firewall problem and that group's
/// rules, else `Other`.
pub fn mode(facts: &EnvFacts, problems: &[EnvProblem]) -> Option<FirewallMode> {
    if facts.firewall.contains(&None) {
        return None;
    }
    if facts.firewall == [Some(false); 3] {
        return Some(FirewallMode::Off);
    }
    if facts.firewall == [Some(true); 3] && problems.is_empty() {
        match facts.firewall_rules.as_ref().and_then(BlockRules::group) {
            Some(FirewallGroup::Dstack155) => return Some(FirewallMode::Dstack155),
            Some(FirewallGroup::DanteLeg) => return Some(FirewallMode::DanteLeg),
            None => {}
        }
    }
    Some(FirewallMode::Other)
}

#[cfg(test)]
#[path = "prod_env_firewall_tests.rs"]
mod tests;
