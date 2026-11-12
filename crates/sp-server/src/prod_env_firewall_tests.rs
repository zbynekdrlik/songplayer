//! #255 (dstack#155): the firewall's two accepted states and every way a box
//! misses them.

use sp_core::prod_env::firewall::{FirewallDefaults, FirewallMode};
use sp_core::prod_env::{EnvProblem, FirewallProfile, Unreadable};

use super::*;
use crate::prod_env::{EnvFacts, verdict};

const ALLOW: FirewallDefaults = FirewallDefaults {
    inbound: 2,
    outbound: 2,
};

/// Windows' own default actions: inbound Block, outbound Allow.
const WINDOWS: FirewallDefaults = FirewallDefaults {
    inbound: 4,
    outbound: 2,
};

fn rules(in_group: u32, outside: &[&str]) -> Option<BlockRules> {
    Some(BlockRules {
        in_group,
        outside: outside.iter().map(|n| n.to_string()).collect(),
    })
}

/// The firewall facts alone (the rest of the box holds).
fn facts(
    firewall: [Option<bool>; 3],
    defaults: [Option<FirewallDefaults>; 3],
    block: Option<BlockRules>,
) -> EnvFacts {
    EnvFacts {
        firewall,
        firewall_defaults: defaults,
        firewall_rules: block,
        ..EnvFacts::default()
    }
}

/// dstack's standard as resolume-snv read it (10.11.2026 12:00Z).
fn standard() -> EnvFacts {
    facts([Some(true); 3], [Some(ALLOW); 3], rules(16, &[]))
}

use FirewallProfile::{Domain, Private, Public};

#[test]
fn every_profile_off_holds_whatever_its_rules_and_defaults() {
    for block in [rules(0, &[]), rules(3, &["Block X"]), None] {
        for defaults in [[Some(WINDOWS); 3], [Some(ALLOW); 3], [None; 3]] {
            let f = facts([Some(false); 3], defaults, block.clone());
            assert_eq!(problems(&f), [], "{block:?} {defaults:?}");
            assert_eq!(mode(&f, &problems(&f)), Some(FirewallMode::Off));
        }
    }
}

#[test]
fn dstacks_standard_holds() {
    let f = standard();
    assert_eq!(problems(&f), []);
    assert_eq!(mode(&f, &problems(&f)), Some(FirewallMode::Dstack155));
    // Through the whole verdict: no firewall problem.
    assert!(
        !verdict(&f)
            .problems
            .iter()
            .any(|p| p.code().starts_with("firewall")),
        "{:?}",
        verdict(&f).problems
    );
}

/// A firewall that is on and follows nothing of the standard (no group rule,
/// no default Allow): `firewall_on` for each profile on, nothing else, even
/// with the others off and Block rules elsewhere.
#[test]
fn a_profile_on_with_nothing_of_the_standard_is_a_firewall_that_is_on() {
    let all = facts([Some(true); 3], [Some(WINDOWS); 3], rules(0, &["Block X"]));
    assert_eq!(
        problems(&all),
        [
            EnvProblem::FirewallOn(Domain),
            EnvProblem::FirewallOn(Private),
            EnvProblem::FirewallOn(Public),
        ]
    );
    assert_eq!(mode(&all, &problems(&all)), Some(FirewallMode::Other));
    // An OFF profile's Allow defaults are no attempt at the standard.
    let one = facts(
        [Some(false), Some(true), Some(false)],
        [Some(ALLOW), Some(WINDOWS), Some(ALLOW)],
        rules(0, &[]),
    );
    assert_eq!(problems(&one), [EnvProblem::FirewallOn(Private)]);
    assert_eq!(mode(&one, &problems(&one)), Some(FirewallMode::Other));
}

#[test]
fn the_standard_with_a_profile_off_names_that_profile() {
    for (i, profile) in FirewallProfile::ALL.into_iter().enumerate() {
        let mut f = standard();
        f.firewall[i] = Some(false);
        assert_eq!(problems(&f), [EnvProblem::FirewallOff(profile)]);
        assert_eq!(mode(&f, &problems(&f)), Some(FirewallMode::Other));
    }
}

#[test]
fn the_standard_with_a_default_that_blocks_names_the_profile_and_the_actions() {
    let inbound = FirewallDefaults {
        inbound: 4,
        outbound: 2,
    };
    let outbound = FirewallDefaults {
        inbound: 2,
        outbound: 4,
    };
    let unset = FirewallDefaults {
        inbound: 0,
        outbound: 2,
    };
    for (i, profile) in FirewallProfile::ALL.into_iter().enumerate() {
        for defaults in [inbound, outbound, unset] {
            let mut f = standard();
            f.firewall_defaults[i] = Some(defaults);
            assert_eq!(
                problems(&f),
                [EnvProblem::FirewallDefaultBlock { profile, defaults }]
            );
            assert_eq!(mode(&f, &problems(&f)), Some(FirewallMode::Other));
        }
        let mut f = standard();
        f.firewall_defaults[i] = None;
        assert_eq!(
            problems(&f),
            [EnvProblem::Unreadable(Unreadable::FirewallDefaults(
                profile
            ))]
        );
    }
}

#[test]
fn a_block_rule_outside_the_group_is_named() {
    let f = facts(
        [Some(true); 3],
        [Some(ALLOW); 3],
        rules(16, &["Block X", "Block Y"]),
    );
    assert_eq!(
        problems(&f),
        [EnvProblem::FirewallBlockOutsideGroup(vec![
            "Block X".to_string(),
            "Block Y".to_string()
        ])]
    );
}

#[test]
fn the_group_holds_exactly_fourteen_enabled_block_rules() {
    for count in [0, 1, 13, 15] {
        let f = facts([Some(true); 3], [Some(ALLOW); 3], rules(count, &[]));
        assert_eq!(
            problems(&f),
            [EnvProblem::FirewallGroupIncomplete { rules: count }],
            "{count}"
        );
    }
    // A group rule alone is an attempt: Windows' defaults then fail.
    let f = facts(
        [Some(true), Some(true), Some(false)],
        [Some(WINDOWS); 3],
        rules(3, &["Block X"]),
    );
    assert_eq!(
        problems(&f),
        [
            EnvProblem::FirewallDefaultBlock {
                profile: Domain,
                defaults: WINDOWS
            },
            EnvProblem::FirewallDefaultBlock {
                profile: Private,
                defaults: WINDOWS
            },
            EnvProblem::FirewallOff(Public),
            EnvProblem::FirewallBlockOutsideGroup(vec!["Block X".to_string()]),
            EnvProblem::FirewallGroupIncomplete { rules: 3 },
        ]
    );
}

/// The rules not read with a profile on: never taken as the standard, the
/// box is judged in full.
#[test]
fn unread_rules_with_a_profile_on_are_unreadable() {
    let f = facts([Some(true); 3], [Some(ALLOW); 3], None);
    assert_eq!(
        problems(&f),
        [EnvProblem::Unreadable(Unreadable::FirewallRules)]
    );
    assert_eq!(mode(&f, &problems(&f)), Some(FirewallMode::Other));
    let f = facts(
        [Some(true), Some(false), Some(false)],
        [Some(WINDOWS), None, None],
        None,
    );
    assert_eq!(
        problems(&f),
        [
            EnvProblem::FirewallDefaultBlock {
                profile: Domain,
                defaults: WINDOWS
            },
            EnvProblem::FirewallOff(Private),
            EnvProblem::FirewallOff(Public),
            EnvProblem::Unreadable(Unreadable::FirewallRules),
        ]
    );
}

#[test]
fn an_unread_profile_is_named_and_nothing_else_is_judged() {
    let mut f = standard();
    f.firewall = [None, Some(true), None];
    f.firewall_rules = rules(0, &["Block X"]);
    assert_eq!(
        problems(&f),
        [
            EnvProblem::Unreadable(Unreadable::Firewall(Domain)),
            EnvProblem::Unreadable(Unreadable::Firewall(Public)),
        ]
    );
    assert_eq!(mode(&f, &problems(&f)), None);
}

#[test]
fn a_rule_is_dstacks_by_its_group_or_its_display_group() {
    assert!(in_dstack_group(Some("dstack155"), None));
    assert!(in_dstack_group(None, Some("dstack155")));
    assert!(in_dstack_group(Some("Other"), Some(" DSTACK155 ")));
    assert!(in_dstack_group(Some(" Dstack155"), Some("Other")));
    assert!(!in_dstack_group(None, None));
    assert!(!in_dstack_group(
        Some("dstack1550"),
        Some("Core Networking")
    ));
}
