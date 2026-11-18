//! #255 (dstack#155), #322 (dstack#264): the firewall's codes and Slovak
//! texts, its default actions, its groups, its mode on the wire.

use super::super::firewall::{
    ACTION_ALLOW, ACTION_BLOCK, ACTION_NOT_CONFIGURED, DANTE_LEG_GROUP, DANTE_LEG_GROUP_RULES,
    DANTE_LEG_RULE_NAMES, DSTACK_GROUP, DSTACK_GROUP_RULES, FirewallDefaults, FirewallGroup,
    FirewallMode, action_sk, names_sk,
};
use super::super::{EnvProblem, FirewallProfile, Unreadable};

fn defaults(inbound: u32, outbound: u32) -> FirewallDefaults {
    FirewallDefaults { inbound, outbound }
}

/// Every firewall problem of the two-network standard, one of each kind.
fn every_firewall_problem() -> Vec<EnvProblem> {
    vec![
        EnvProblem::FirewallOff(FirewallProfile::Domain),
        EnvProblem::FirewallOff(FirewallProfile::Public),
        EnvProblem::FirewallDefaultBlock {
            profile: FirewallProfile::Private,
            defaults: defaults(4, 2),
        },
        EnvProblem::FirewallDefaultBlock {
            profile: FirewallProfile::Public,
            defaults: defaults(0, 7),
        },
        EnvProblem::FirewallBlockOutsideGroup(vec!["Block X".to_string()]),
        EnvProblem::FirewallGroupIncomplete {
            group: FirewallGroup::Dstack155,
            rules: 13,
        },
        EnvProblem::FirewallGroupIncomplete {
            group: FirewallGroup::DanteLeg,
            rules: 9,
        },
        EnvProblem::FirewallGroupsMixed {
            dstack155: 14,
            dante_leg: 10,
        },
        EnvProblem::Unreadable(Unreadable::FirewallDefaults(FirewallProfile::Private)),
        EnvProblem::Unreadable(Unreadable::FirewallRules),
    ]
}

#[test]
fn the_standards_values_are_dstacks() {
    assert_eq!(DSTACK_GROUP, "dstack155");
    assert_eq!(DSTACK_GROUP_RULES, 14);
    assert_eq!(DANTE_LEG_GROUP, "dstack Dante leg");
    assert_eq!(DANTE_LEG_GROUP_RULES, 10);
    assert_eq!(DANTE_LEG_RULE_NAMES.len() as u32, DANTE_LEG_GROUP_RULES);
    assert_eq!(
        (
            FirewallGroup::Dstack155.name(),
            FirewallGroup::Dstack155.rules()
        ),
        ("dstack155", 14)
    );
    assert_eq!(
        (
            FirewallGroup::DanteLeg.name(),
            FirewallGroup::DanteLeg.rules()
        ),
        ("dstack Dante leg", 10)
    );
    assert_eq!(
        (ACTION_NOT_CONFIGURED, ACTION_ALLOW, ACTION_BLOCK),
        (0, 2, 4)
    );
}

#[test]
fn every_firewall_problem_has_its_stable_code() {
    let codes: Vec<String> = every_firewall_problem()
        .iter()
        .map(EnvProblem::code)
        .collect();
    assert_eq!(
        codes,
        [
            "firewall_off:domain",
            "firewall_off:public",
            "firewall_default_block:private",
            "firewall_default_block:public",
            "firewall_block_outside_group",
            "firewall_group_incomplete",
            "firewall_group_incomplete",
            "firewall_groups_mixed",
            "unreadable:firewall_defaults:private",
            "unreadable:firewall_rules",
        ]
    );
}

#[test]
fn every_firewall_problem_reads_in_slovak() {
    let texts: Vec<String> = every_firewall_problem()
        .iter()
        .map(EnvProblem::text_sk)
        .collect();
    assert_eq!(
        texts,
        [
            "brána firewall je vypnutá (doménový profil), hoci štandard dstack je nastavený — \
             má byť zapnutá vo všetkých profiloch",
            "brána firewall je vypnutá (verejný profil), hoci štandard dstack je nastavený — \
             má byť zapnutá vo všetkých profiloch",
            "brána firewall (súkromný profil): predvolená akcia má byť povoliť (prichádzajúce: \
             blokovať, odchádzajúce: povoliť)",
            "brána firewall (verejný profil): predvolená akcia má byť povoliť (prichádzajúce: \
             nenastavené, odchádzajúce: hodnota 7)",
            "brána firewall má zapnuté blokovacie pravidlá mimo skupín dstack („dstack Dante \
             leg“, dstack155): „Block X“",
            "skupina pravidiel „dstack155“ má 13 zapnutých blokovacích pravidiel (má ich mať 14)",
            "skupina pravidiel „dstack Dante leg“ má 9 zapnutých blokovacích pravidiel (má ich \
             mať 10)",
            "brána firewall má zapnuté obe skupiny pravidiel dstack („dstack Dante leg“: 10, \
             dstack155: 14) — prechod na novú skupinu nie je dokončený, platiť má len jedna",
            "predvolené akcie brány firewall (súkromný profil) sa nepodarilo zistiť",
            "blokovacie pravidlá brány firewall sa nepodarilo zistiť",
        ]
    );
}

#[test]
fn only_allow_both_ways_is_allow() {
    assert!(defaults(2, 2).allow());
    assert!(!defaults(4, 2).allow());
    assert!(!defaults(2, 4).allow());
    assert!(!defaults(0, 2).allow());
    assert!(!defaults(2, 0).allow());
}

#[test]
fn a_default_action_reads_in_slovak() {
    assert_eq!(action_sk(2), "povoliť");
    assert_eq!(action_sk(4), "blokovať");
    assert_eq!(action_sk(0), "nenastavené");
    assert_eq!(action_sk(3), "hodnota 3");
}

#[test]
fn rule_names_are_listed_up_to_five_then_counted() {
    let names = |n: usize| -> Vec<String> { (1..=n).map(|i| format!("R{i}")).collect() };
    assert_eq!(names_sk(&names(1)), "„R1“");
    assert_eq!(names_sk(&names(5)), "„R1“, „R2“, „R3“, „R4“, „R5“");
    assert_eq!(
        names_sk(&names(6)),
        "„R1“, „R2“, „R3“, „R4“, „R5“ a ďalšie 1"
    );
    assert_eq!(
        names_sk(&names(8)),
        "„R1“, „R2“, „R3“, „R4“, „R5“ a ďalšie 3"
    );
}

#[test]
fn the_mode_is_named_on_the_wire() {
    for (mode, text) in [
        (FirewallMode::Off, "off"),
        (FirewallMode::Dstack155, "dstack155"),
        (FirewallMode::DanteLeg, "dante_leg"),
        (FirewallMode::Other, "other"),
    ] {
        let json = serde_json::to_value(mode).unwrap();
        assert_eq!(json, serde_json::json!(text));
        assert_eq!(serde_json::from_value::<FirewallMode>(json).unwrap(), mode);
    }
}

/// An older server's status (no mode, no defaults, no rules) reads as not
/// read.
#[test]
fn an_older_firewall_status_reads_the_new_facts_as_not_read() {
    let old: super::super::FirewallState = serde_json::from_value(
        serde_json::json!({"domain": false, "private": false, "public": false}),
    )
    .unwrap();
    assert_eq!(old.mode, None);
    assert_eq!(old.defaults, [None; 3]);
    assert_eq!(old.dstack_group_rules, None);
    assert_eq!(old.dante_leg_group_rules, None);
    assert_eq!(old.block_rules_outside_group, None);
}

/// #322: a rule is a group's by its `RuleGroup` or its `DisplayGroup`,
/// trimmed, ASCII case ignored; anything else is no group of dstack's.
#[test]
fn a_rule_belongs_to_a_group_by_either_name() {
    use FirewallGroup::{DanteLeg, Dstack155};
    for (rule, display, want) in [
        (Some("dstack155"), None, Some(Dstack155)),
        (None, Some("dstack155"), Some(Dstack155)),
        (Some(" DSTACK155 "), Some("other"), Some(Dstack155)),
        (Some("other"), Some("dstack155"), Some(Dstack155)),
        (
            Some("dstack Dante leg"),
            Some("dstack Dante leg"),
            Some(DanteLeg),
        ),
        (None, Some(" DSTACK dante LEG"), Some(DanteLeg)),
        (Some("dstack Dante leg"), None, Some(DanteLeg)),
        (Some("dstack Dante"), Some("dstack1555"), None),
        (Some(""), None, None),
        (None, None, None),
    ] {
        assert_eq!(
            FirewallGroup::of(rule, display),
            want,
            "{rule:?} {display:?}"
        );
    }
}

/// dstack v0.6.123's `dante-leg.ps1` names every rule `<group>: <what>`,
/// each name once.
#[test]
fn every_generic_rule_is_named_after_its_group() {
    for name in DANTE_LEG_RULE_NAMES {
        assert!(name.starts_with("dstack Dante leg: "), "{name}");
    }
    let mut unique = DANTE_LEG_RULE_NAMES.to_vec();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), 10);
}
