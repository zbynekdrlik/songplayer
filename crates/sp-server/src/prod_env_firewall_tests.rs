//! #255 (dstack#155), #322 (dstack#264): the firewall's three accepted
//! states and every way a box misses them.

use sp_core::prod_env::firewall::{FirewallDefaults, FirewallGroup, FirewallMode};
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

/// `dstack155` rules of the per-box standard and the named rules outside.
fn rules(dstack155: u32, outside: &[&str]) -> Option<BlockRules> {
    Some(BlockRules {
        dstack155,
        dante_leg: 0,
        outside: outside.iter().map(|n| n.to_string()).collect(),
    })
}

/// #322: rules of both groups and none outside.
fn groups(dstack155: u32, dante_leg: u32) -> Option<BlockRules> {
    Some(BlockRules {
        dstack155,
        dante_leg,
        outside: Vec::new(),
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
    facts([Some(true); 3], [Some(ALLOW); 3], rules(14, &[]))
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
        rules(14, &["Block X", "Block Y"]),
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
    for count in [1, 13, 15] {
        let f = facts([Some(true); 3], [Some(ALLOW); 3], rules(count, &[]));
        assert_eq!(
            problems(&f),
            [EnvProblem::FirewallGroupIncomplete {
                group: FirewallGroup::Dstack155,
                rules: count
            }],
            "{count}"
        );
    }
    // No rule of either group: judged against the generic group, the one
    // dstack sets up now.
    let f = facts([Some(true); 3], [Some(ALLOW); 3], rules(0, &[]));
    assert_eq!(
        problems(&f),
        [EnvProblem::FirewallGroupIncomplete {
            group: FirewallGroup::DanteLeg,
            rules: 0
        }]
    );
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
            EnvProblem::FirewallGroupIncomplete {
                group: FirewallGroup::Dstack155,
                rules: 3
            },
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

/// #322 (dstack#264): dstack's generic Dante-leg filter as v0.6.123's
/// `dante-leg.ps1` leaves resolume-snv: every profile on with Allow both
/// ways, the group's 10 rules, nothing else.
fn generic() -> EnvFacts {
    facts([Some(true); 3], [Some(ALLOW); 3], groups(0, 10))
}

#[test]
fn dstacks_generic_filter_holds() {
    let f = generic();
    assert_eq!(problems(&f), []);
    assert_eq!(mode(&f, &problems(&f)), Some(FirewallMode::DanteLeg));
    assert!(
        !verdict(&f)
            .problems
            .iter()
            .any(|p| p.code().starts_with("firewall")),
        "{:?}",
        verdict(&f).problems
    );
}

#[test]
fn the_generic_group_holds_exactly_ten_enabled_block_rules() {
    for count in [1, 9, 11, 14] {
        let f = facts([Some(true); 3], [Some(ALLOW); 3], groups(0, count));
        assert_eq!(
            problems(&f),
            [EnvProblem::FirewallGroupIncomplete {
                group: FirewallGroup::DanteLeg,
                rules: count
            }],
            "{count}"
        );
        assert_eq!(mode(&f, &problems(&f)), Some(FirewallMode::Other));
    }
}

/// A generic rule alone is an attempt too: Windows' defaults then fail.
#[test]
fn a_generic_rule_alone_is_an_attempt() {
    let f = facts(
        [Some(true), Some(true), Some(false)],
        [Some(WINDOWS); 3],
        groups(0, 10),
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
        ]
    );
}

/// Both groups holding rules (the switch half done, or `-RemoveGroup`
/// forgotten): one named problem, never either standard.
#[test]
fn both_groups_at_once_are_named() {
    for (old, new) in [(14, 10), (1, 10), (14, 1), (3, 7)] {
        let f = facts([Some(true); 3], [Some(ALLOW); 3], groups(old, new));
        assert_eq!(
            problems(&f),
            [EnvProblem::FirewallGroupsMixed {
                dstack155: old,
                dante_leg: new
            }],
            "{old} {new}"
        );
        assert_eq!(mode(&f, &problems(&f)), Some(FirewallMode::Other));
    }
}

#[test]
fn a_block_rule_outside_both_groups_is_named_with_the_generic_filter() {
    let mut f = generic();
    f.firewall_rules = Some(BlockRules {
        dstack155: 0,
        dante_leg: 10,
        outside: vec!["Block X".to_string()],
    });
    assert_eq!(
        problems(&f),
        [EnvProblem::FirewallBlockOutsideGroup(vec![
            "Block X".to_string()
        ])]
    );
    assert_eq!(mode(&f, &problems(&f)), Some(FirewallMode::Other));
}

/// The per-box standard still holds while a PC waits for its switch.
#[test]
fn the_per_box_standard_is_still_its_own_mode() {
    let f = facts([Some(true); 3], [Some(ALLOW); 3], groups(14, 0));
    assert_eq!(mode(&f, &problems(&f)), Some(FirewallMode::Dstack155));
}

#[test]
fn a_rule_is_counted_by_its_group_and_named_outside_both() {
    let mut r = BlockRules::default();
    r.add(Some(FirewallGroup::Dstack155), "a".to_string());
    r.add(Some(FirewallGroup::DanteLeg), "b".to_string());
    r.add(Some(FirewallGroup::DanteLeg), "c".to_string());
    r.add(None, "Block X".to_string());
    assert_eq!(
        r,
        BlockRules {
            dstack155: 1,
            dante_leg: 2,
            outside: vec!["Block X".to_string()],
        }
    );
}

#[test]
fn the_rules_are_judged_by_the_group_that_holds_them() {
    let group = |old, new| groups(old, new).unwrap().group();
    assert_eq!(group(0, 0), Some(FirewallGroup::DanteLeg));
    assert_eq!(group(0, 3), Some(FirewallGroup::DanteLeg));
    assert_eq!(group(2, 0), Some(FirewallGroup::Dstack155));
    assert_eq!(group(2, 3), None);
}
