//! #255 (dstack#155), #322 (dstack#264): the firewall's two accepted
//! states and every way a box misses them.

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

/// That many rules of the group `dstack Dante leg` and the named rules
/// outside it.
fn rules(dante_leg: u32, outside: &[&str]) -> Option<BlockRules> {
    Some(BlockRules {
        dante_leg,
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

/// #322 (dstack#264): dstack's generic Dante-leg filter as `dante-leg.ps1`
/// leaves resolume-snv and resolume-pp (both read 19.11.2026): every
/// profile on with Allow both ways, the group's 10 rules, nothing else.
fn standard() -> EnvFacts {
    facts([Some(true); 3], [Some(ALLOW); 3], rules(10, &[]))
}

use FirewallProfile::{Domain, Private, Public};

/// #340: the same firewall facts on a PC with a Dante leg (resolume-snv's
/// two networks: an enabled Dante output on 10.77.7.107).
fn with_leg(facts: EnvFacts) -> EnvFacts {
    EnvFacts {
        network: crate::prod_env::network::snv_example(),
        ..facts
    }
}

/// The firewall's problems in a whole verdict, by code.
fn firewall_codes(facts: &EnvFacts) -> Vec<String> {
    verdict(facts)
        .problems
        .iter()
        .map(EnvProblem::code)
        .filter(|code| code.contains("firewall"))
        .collect()
}

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
fn dstacks_generic_filter_holds() {
    let f = standard();
    assert_eq!(problems(&f), []);
    assert_eq!(mode(&f, &problems(&f)), Some(FirewallMode::DanteLeg));
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
        rules(10, &["Block X", "Block Y"]),
    );
    assert_eq!(
        problems(&f),
        [EnvProblem::FirewallBlockOutsideGroup(vec![
            "Block X".to_string(),
            "Block Y".to_string()
        ])]
    );
    assert_eq!(mode(&f, &problems(&f)), Some(FirewallMode::Other));
}

#[test]
fn the_group_holds_exactly_ten_enabled_block_rules() {
    for count in [1, 9, 11, 14] {
        let f = facts([Some(true); 3], [Some(ALLOW); 3], rules(count, &[]));
        assert_eq!(
            problems(&f),
            [EnvProblem::FirewallGroupIncomplete { rules: count }],
            "{count}"
        );
        assert_eq!(mode(&f, &problems(&f)), Some(FirewallMode::Other));
    }
    // No rule of the group, the defaults allowing: the standard, incomplete.
    let f = facts([Some(true); 3], [Some(ALLOW); 3], rules(0, &[]));
    assert_eq!(
        problems(&f),
        [EnvProblem::FirewallGroupIncomplete { rules: 0 }]
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
            EnvProblem::FirewallGroupIncomplete { rules: 3 },
        ]
    );
}

/// The whole group alone is an attempt too: Windows' defaults then fail,
/// and nothing of the rules does.
#[test]
fn the_whole_group_alone_is_an_attempt() {
    let f = facts(
        [Some(true), Some(true), Some(false)],
        [Some(WINDOWS); 3],
        rules(10, &[]),
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

/// #322: dstack#155's per-box group `dstack155` is no longer accepted. Its
/// rules are no rules of the group, so the reader names them outside it
/// (`prod_env_read_tests_firewall.rs`), and they are judged like any other
/// foreign Block rule.
#[test]
fn rules_of_the_old_group_are_block_rules_outside_the_group() {
    // A PC that never switched: the old group's 14 rules, Allow defaults.
    let old: Vec<String> = (1..=14).map(|i| format!("dstack155 rule {i}")).collect();
    let never_switched = facts(
        [Some(true); 3],
        [Some(ALLOW); 3],
        Some(BlockRules {
            dante_leg: 0,
            outside: old.clone(),
        }),
    );
    assert_eq!(
        problems(&never_switched),
        [
            EnvProblem::FirewallBlockOutsideGroup(old),
            EnvProblem::FirewallGroupIncomplete { rules: 0 },
        ]
    );
    assert_eq!(
        mode(&never_switched, &problems(&never_switched)),
        Some(FirewallMode::Other)
    );
    // A switch that left one old rule behind: the generic group is whole,
    // the leftover is named.
    let leftover = facts(
        [Some(true); 3],
        [Some(ALLOW); 3],
        rules(10, &["dstack155 obs64.exe in"]),
    );
    assert_eq!(
        problems(&leftover),
        [EnvProblem::FirewallBlockOutsideGroup(vec![
            "dstack155 obs64.exe in".to_string()
        ])]
    );
    assert_eq!(
        mode(&leftover, &problems(&leftover)),
        Some(FirewallMode::Other)
    );
    // Old-group rules are no attempt at the standard: with Windows' own
    // defaults and no rule of the group the firewall is simply on.
    let plain = facts(
        [Some(true); 3],
        [Some(WINDOWS); 3],
        rules(0, &["dstack155 obs64.exe in", "dstack155 TCP out"]),
    );
    assert_eq!(
        problems(&plain),
        [
            EnvProblem::FirewallOn(Domain),
            EnvProblem::FirewallOn(Private),
            EnvProblem::FirewallOn(Public),
        ]
    );
    assert_eq!(mode(&plain, &problems(&plain)), Some(FirewallMode::Other));
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

/// #340: with a Dante leg, every profile off is ONE problem of its own,
/// whatever the rules and the default actions read (off, they filter
/// nothing); the mode still names the state the profiles are in.
#[test]
fn every_profile_off_with_a_dante_leg_is_one_problem() {
    for block in [rules(0, &[]), rules(10, &[]), rules(3, &["Block X"]), None] {
        for defaults in [[Some(WINDOWS); 3], [Some(ALLOW); 3], [None; 3]] {
            let f = with_leg(facts([Some(false); 3], defaults, block.clone()));
            assert_eq!(
                problems(&f),
                [EnvProblem::FirewallOffWithDanteLeg],
                "{block:?} {defaults:?}"
            );
            assert_eq!(mode(&f, &problems(&f)), Some(FirewallMode::Off));
        }
    }
    // Through the whole verdict: exactly that one, and the box fails.
    let f = with_leg(facts([Some(false); 3], [Some(ALLOW); 3], rules(10, &[])));
    assert_eq!(firewall_codes(&f), ["firewall_off_with_dante_leg"]);
    assert!(!verdict(&f).ok);
}

/// #340: a PC with no Dante leg keeps "off = clean" (the owner,
/// 11.11.2026: Dante is one audio path, a PC without it passes clean).
/// The outputs unread is no leg either: that is its own problem.
#[test]
fn every_profile_off_with_no_dante_leg_holds() {
    use crate::prod_env::network::{NetworkFacts, has_leg, snv_example};
    let off = facts([Some(false); 3], [Some(WINDOWS); 3], rules(0, &[]));
    assert!(!has_leg(&off.network));
    assert_eq!(firewall_codes(&off), Vec::<String>::new());
    // SNV's adapters and routes with no enabled Dante output.
    let no_output = EnvFacts {
        network: NetworkFacts {
            dante_ips: Some(Vec::new()),
            ..snv_example()
        },
        ..off.clone()
    };
    assert!(!has_leg(&no_output.network));
    assert_eq!(problems(&no_output), []);
    let unread = EnvFacts {
        network: NetworkFacts {
            dante_ips: None,
            ..snv_example()
        },
        ..off
    };
    assert!(!has_leg(&unread.network));
    assert_eq!(problems(&unread), []);
    assert!(has_leg(&snv_example()));
}

/// #340: both production PCs today (a Dante leg, dstack's standard) read
/// clean.
#[test]
fn dstacks_standard_with_a_dante_leg_holds() {
    let f = with_leg(standard());
    assert_eq!(problems(&f), []);
    assert_eq!(mode(&f, &problems(&f)), Some(FirewallMode::DanteLeg));
    assert_eq!(firewall_codes(&f), Vec::<String>::new());
}

/// #340: a firewall that is partly on, or unread, reads on a PC with a
/// Dante leg exactly as on one without: the new problem is for every
/// profile off only.
#[test]
fn a_dante_leg_changes_no_other_firewall_verdict() {
    let mut one_off = standard();
    one_off.firewall[2] = Some(false);
    let cases = [
        // Simply on: nothing of the standard.
        facts([Some(true); 3], [Some(WINDOWS); 3], rules(0, &["Block X"])),
        facts(
            [Some(false), Some(true), Some(false)],
            [Some(WINDOWS); 3],
            rules(0, &[]),
        ),
        // The standard with a profile off, a default that blocks, a rule
        // outside the group, a group that is short.
        one_off,
        facts([Some(true); 3], [Some(WINDOWS); 3], rules(10, &[])),
        facts([Some(true); 3], [Some(ALLOW); 3], rules(10, &["Block X"])),
        facts([Some(true); 3], [Some(ALLOW); 3], rules(9, &[])),
        facts([Some(true); 3], [Some(ALLOW); 3], None),
        // A profile unread, the others off.
        facts([None, Some(false), Some(false)], [None; 3], rules(0, &[])),
    ];
    for f in cases {
        let plain = problems(&f);
        assert!(!plain.is_empty(), "{f:?}");
        let leg = with_leg(f.clone());
        assert_eq!(problems(&leg), plain, "{:?}", f.firewall);
        assert!(
            !plain.contains(&EnvProblem::FirewallOffWithDanteLeg),
            "{:?}",
            f.firewall
        );
        assert_eq!(mode(&leg, &problems(&leg)), mode(&f, &plain));
    }
}

/// #340: `scripts/setup-prod-pc.ps1` blocks on the same state under the
/// same code, by its own Dante-leg fact (each line matched alone: a
/// Windows checkout holds the script with CRLF line ends).
#[test]
fn the_setup_script_blocks_a_dante_leg_with_the_firewall_off() {
    let script = include_str!("../../../scripts/setup-prod-pc.ps1");
    let code = EnvProblem::FirewallOffWithDanteLeg.code();
    assert_eq!(code, "firewall_off_with_dante_leg");
    let blocker = format!("New-ProdPcBlocker '{code}' ");
    let lines: Vec<&str> = script.lines().filter(|l| l.contains(&blocker)).collect();
    assert_eq!(lines.len(), 1, "{blocker}");
    assert_eq!(
        script
            .lines()
            .filter(|l| l.trim() == "if (Test-ProdPcDanteLeg $Facts) {")
            .count(),
        1
    );
}

#[test]
fn a_rule_is_counted_in_the_group_or_named_outside_it() {
    let mut r = BlockRules::default();
    r.add(true, "a".to_string());
    r.add(true, "b".to_string());
    r.add(false, "Block X".to_string());
    assert_eq!(
        r,
        BlockRules {
            dante_leg: 2,
            outside: vec!["Block X".to_string()],
        }
    );
}

/// #322: `scripts/setup-prod-pc.ps1` judges the same rules by name and the
/// same group.
#[test]
fn the_setup_script_knows_dstacks_generic_rules() {
    use sp_core::prod_env::firewall::{DANTE_LEG_GROUP, DANTE_LEG_RULE_NAMES};
    // A Windows checkout holds the script with CRLF line ends.
    let script = include_str!("../../../scripts/setup-prod-pc.ps1").replace("\r\n", "\n");
    let quoted: Vec<String> = DANTE_LEG_RULE_NAMES
        .iter()
        .map(|n| format!("'{n}'"))
        .collect();
    let names = format!("return @({})", quoted.join(", "));
    assert!(script.contains(&names), "{names}");
    let group = format!("function Get-ProdPcFirewallGroup {{\n    return '{DANTE_LEG_GROUP}'\n}}");
    assert!(script.contains(&group), "{group}");
}
