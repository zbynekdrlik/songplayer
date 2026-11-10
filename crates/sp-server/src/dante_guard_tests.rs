//! #255 item C: the pure Dante-guard rules (CIDR, the refused set).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::{Duration, Instant};

use sp_core::audio_outputs::{DanteDest, OutputEntry, VbanDest};

use super::{Cidr, WARN_EVERY, dante_ips, env_subnets, parse_list, refused_set, refuses, warn_due};

fn ip(s: &str) -> Ipv4Addr {
    s.parse().unwrap()
}

fn cidr(s: &str) -> Cidr {
    Cidr::parse(s).unwrap_or_else(|| panic!("{s} parses"))
}

#[test]
fn a_cidr_keeps_its_network_and_contains_exactly_its_addresses() {
    let dante = cidr("10.77.7.107/24");
    assert_eq!(dante.to_string(), "10.77.7.0/24");
    assert!(dante.contains(ip("10.77.7.107")));
    assert!(dante.contains(ip("10.77.7.0")));
    assert!(dante.contains(ip("10.77.7.255")));
    assert!(!dante.contains(ip("10.77.9.201")));
    assert!(!dante.contains(ip("10.77.6.255")));
    assert!(!dante.contains(ip("10.77.8.0")));
}

#[test]
fn the_prefix_bounds_hold_at_0_and_32() {
    let all = cidr("192.168.1.1/0");
    assert_eq!(all.to_string(), "0.0.0.0/0");
    assert!(all.contains(ip("8.8.8.8")));
    let one = cidr("10.0.0.5/32");
    assert_eq!(one.to_string(), "10.0.0.5/32");
    assert!(one.contains(ip("10.0.0.5")));
    assert!(!one.contains(ip("10.0.0.4")));
    let odd = cidr("10.77.7.200/25");
    assert_eq!(odd.to_string(), "10.77.7.128/25");
    assert!(!odd.contains(ip("10.77.7.127")));
    assert!(Cidr::new(ip("10.0.0.0"), 32).is_some());
    assert!(Cidr::new(ip("10.0.0.0"), 33).is_none());
    assert_eq!(Cidr::new(ip("127.0.0.1"), 8), Some(cidr("127.0.0.0/8")));
}

#[test]
fn parse_takes_spaces_and_refuses_anything_else() {
    assert_eq!(Cidr::parse(" 10.77.7.0 / 24 "), Some(cidr("10.77.7.0/24")));
    for bad in [
        "10.77.7.0",
        "10.77.7.0/33",
        "10.77.7/24",
        "x/24",
        "10.0.0.0/-1",
        "::1/64",
        "",
    ] {
        assert_eq!(Cidr::parse(bad), None, "{bad:?}");
    }
}

#[test]
fn a_list_is_all_or_nothing_and_blank_items_are_skipped() {
    assert_eq!(
        parse_list("10.77.7.0/24, 192.168.70.0/24 ,"),
        Ok(vec![cidr("10.77.7.0/24"), cidr("192.168.70.0/24")])
    );
    assert_eq!(parse_list(""), Ok(vec![]));
    assert_eq!(parse_list(" , "), Ok(vec![]));
    let err = parse_list("10.77.7.0/24,nonsense").unwrap_err();
    assert!(err.contains("nonsense"), "{err}");
}

#[test]
fn the_env_value_unset_is_none_and_set_is_parsed() {
    assert_eq!(env_subnets(None), Ok(vec![]));
    assert_eq!(
        env_subnets(Some("10.77.7.0/24")),
        Ok(vec![cidr("10.77.7.0/24")])
    );
    assert!(env_subnets(Some("10.77.7.0")).is_err());
}

fn dante(id: &str, addr: &str, enabled: bool) -> OutputEntry {
    let mut e = OutputEntry::dante(
        id,
        "Dante",
        DanteDest {
            device: "songplayer".to_string(),
            ip: addr.to_string(),
        },
    );
    e.enabled = enabled;
    e
}

#[test]
fn only_enabled_dante_outputs_with_a_readable_address_count() {
    let vban = OutputEntry::vban(
        "out-1",
        "FOH",
        VbanDest {
            host: "10.77.9.10".to_string(),
            port: 6980,
            stream_name: "SP".to_string(),
            format: Default::default(),
        },
    );
    let entries = [
        vban,
        dante("out-2", "10.77.7.107", true),
        dante("out-3", "10.77.8.5", false),
        dante("out-4", "not an ip", true),
        dante("out-5", " 192.168.70.2 ", true),
    ];
    assert_eq!(dante_ips(&entries), [ip("10.77.7.107"), ip("192.168.70.2")]);
}

#[test]
fn the_set_is_the_env_plus_each_dante_address_interface_subnet_once() {
    let ifaces = [
        (ip("10.77.9.201"), 24),
        (ip("10.77.7.107"), 24),
        (ip("192.168.70.2"), 16),
    ];
    // Neither: nothing refused.
    assert_eq!(refused_set(&[], &[], &ifaces), vec![]);
    // A Dante output's address takes its interface's prefix.
    assert_eq!(
        refused_set(&[], &[ip("10.77.7.107")], &ifaces),
        vec![cidr("10.77.7.0/24")]
    );
    // The env comes first; a subnet already there is not repeated.
    assert_eq!(
        refused_set(
            &[cidr("10.77.7.0/24")],
            &[ip("10.77.7.107"), ip("192.168.70.2")],
            &ifaces
        ),
        vec![cidr("10.77.7.0/24"), cidr("192.168.0.0/16")]
    );
    // An address no interface holds adds nothing.
    assert_eq!(refused_set(&[], &[ip("10.1.1.1")], &ifaces), vec![]);
    // A bad prefix from the interface list adds nothing.
    assert_eq!(
        refused_set(&[], &[ip("10.1.1.1")], &[(ip("10.1.1.1"), 40)]),
        vec![]
    );
}

#[test]
fn refuses_reads_the_local_address_ipv4_mapped_included() {
    let set = [cidr("10.77.7.0/24"), cidr("127.0.0.0/8")];
    assert!(refuses(&set, IpAddr::V4(ip("10.77.7.107"))));
    assert!(refuses(&set, IpAddr::V4(ip("127.0.0.1"))));
    assert!(!refuses(&set, IpAddr::V4(ip("10.77.9.201"))));
    assert!(refuses(
        &set,
        IpAddr::V6(ip("10.77.7.107").to_ipv6_mapped())
    ));
    assert!(!refuses(
        &set,
        IpAddr::V6(ip("10.77.9.201").to_ipv6_mapped())
    ));
    assert!(!refuses(&set, IpAddr::V6(Ipv6Addr::LOCALHOST)));
    assert!(!refuses(&[], IpAddr::V4(ip("10.77.7.107"))));
}

#[test]
fn a_refusal_warns_first_then_once_a_minute() {
    let t0 = Instant::now();
    assert!(warn_due(None, t0));
    assert!(!warn_due(Some(t0), t0));
    assert!(!warn_due(
        Some(t0),
        t0 + WARN_EVERY - Duration::from_millis(1)
    ));
    assert!(warn_due(Some(t0), t0 + WARN_EVERY));
    assert_eq!(WARN_EVERY, Duration::from_secs(60));
}
