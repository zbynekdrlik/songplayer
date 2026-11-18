//! #255 (dstack#155): the firewall's default actions and its enabled Block
//! rules, read from the active store.

use sp_core::prod_env::FirewallProfile;
use sp_core::prod_env::firewall::FirewallDefaults;

use super::super::*;
use super::{direct, wmi};
use crate::prod_env::firewall::BlockRules;

fn profile(name: &str, inbound: Option<u32>, outbound: Option<u32>) -> FirewallRow {
    FirewallRow {
        name: name.to_string(),
        enabled: Some(1),
        default_inbound: inbound,
        default_outbound: outbound,
    }
}

fn rule(
    id: &str,
    name: Option<&str>,
    group: Option<&str>,
    display: Option<&str>,
) -> FirewallRuleRow {
    FirewallRuleRow {
        id: id.to_string(),
        display_name: name.map(str::to_string),
        rule_group: group.map(str::to_string),
        display_group: display.map(str::to_string),
    }
}

#[test]
fn each_profile_reads_its_own_default_actions() {
    let rows = vec![
        profile("Domain", Some(2), Some(2)),
        profile(" PRIVATE ", Some(4), Some(2)),
        profile("Public", Some(2), None),
    ];
    assert_eq!(
        active_defaults(&rows, FirewallProfile::Domain),
        Some(FirewallDefaults {
            inbound: 2,
            outbound: 2
        })
    );
    assert_eq!(
        active_defaults(&rows, FirewallProfile::Private),
        Some(FirewallDefaults {
            inbound: 4,
            outbound: 2
        })
    );
    // A row without both values, or no row: not read.
    assert_eq!(active_defaults(&rows, FirewallProfile::Public), None);
    assert_eq!(active_defaults(&rows[..1], FirewallProfile::Public), None);
    let no_inbound = vec![profile("Domain", None, Some(2))];
    assert_eq!(active_defaults(&no_inbound, FirewallProfile::Domain), None);
}

#[test]
fn the_block_rules_are_counted_by_group_and_the_rest_named() {
    let read = Ok(WmiRead {
        firewall_rules: Ok(vec![
            rule("{1}", Some("dstack155 obs in"), Some("dstack155"), None),
            rule("{2}", Some("dstack155 obs out"), None, Some("dstack155")),
            rule("{3}", Some("Block X"), Some("Other"), Some("Other")),
            rule("{4}", None, None, None),
            rule(
                "{5}",
                Some("dstack155 tcp in"),
                Some("dstack155"),
                Some("dstack155"),
            ),
            // #322: dstack's generic group, as `dante-leg.ps1` creates it.
            rule(
                "{6}",
                Some("dstack Dante leg: TCP in"),
                Some("dstack Dante leg"),
                Some("dstack Dante leg"),
            ),
            rule(
                "{7}",
                Some("dstack Dante leg: TCP out"),
                None,
                Some("DSTACK DANTE LEG"),
            ),
        ]),
        ..wmi()
    });
    assert_eq!(
        block_rules(&read),
        Ok(BlockRules {
            dstack155: 3,
            dante_leg: 2,
            outside: vec!["Block X".to_string(), "{4}".to_string()],
        })
    );
    assert_eq!(
        facts_from(&direct(), &read).firewall_rules,
        Some(BlockRules {
            dstack155: 3,
            dante_leg: 2,
            outside: vec!["Block X".to_string(), "{4}".to_string()],
        })
    );
}

#[test]
fn block_rules_that_cannot_be_read_say_why() {
    let refused = Ok(WmiRead {
        firewall_rules: Err(0x8004_1017_u32 as i32),
        ..wmi()
    });
    let why = "the firewall's enabled block rules could not be read (0x80041017)";
    assert_eq!(block_rules(&refused), Err(why.to_string()));
    let facts = facts_from(&direct(), &refused);
    assert_eq!(facts.firewall_rules, None);
    assert_eq!(facts.read_errors, [why]);
    let timed_out = "the WMI check did not finish within 30s".to_string();
    assert_eq!(block_rules(&Err(timed_out.clone())), Err(timed_out));
}

/// The default actions come from the active store only: with it unread
/// (the registry decides the profiles) they are not read.
#[test]
fn the_defaults_are_taken_from_the_active_store_only() {
    let read = Ok(WmiRead {
        firewall: Ok(vec![
            profile("Domain", Some(2), Some(2)),
            profile("Private", Some(2), Some(2)),
            profile("Public", Some(4), Some(2)),
        ]),
        ..wmi()
    });
    let allow = FirewallDefaults {
        inbound: 2,
        outbound: 2,
    };
    let block = FirewallDefaults {
        inbound: 4,
        outbound: 2,
    };
    let facts = facts_from(&direct(), &read);
    assert_eq!(facts.firewall, [Some(true); 3]);
    assert_eq!(
        facts.firewall_defaults,
        [Some(allow), Some(allow), Some(block)]
    );
    let refused = Ok(WmiRead {
        firewall: Err(0x8007_0005_u32 as i32),
        ..wmi()
    });
    assert_eq!(facts_from(&direct(), &refused).firewall_defaults, [None; 3]);
    let timed_out = Err("the WMI check did not finish within 30s".to_string());
    assert_eq!(
        facts_from(&direct(), &timed_out).firewall_defaults,
        [None; 3]
    );
}
