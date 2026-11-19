// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The slot table and probe tests are dstack's (`crates/dstack-endpoint/src/
// slots/tests.rs`, tag v0.6.178), kept so the copy stays dstack's rule.

//! #324: the Dante output's slot pick and the named refusal.

use std::cell::RefCell;
use std::net::{Ipv4Addr, SocketAddrV4};

use dlink::Ports;

use super::*;

#[test]
fn slots_count_up_from_dante_s_defaults() {
    assert_eq!(
        slot(0),
        (
            Ports {
                arc: 4440,
                cmc: 8800,
                flows: 4455
            },
            0
        )
    );
    assert_eq!(
        slot(2),
        (
            Ports {
                arc: 4442,
                cmc: 8802,
                flows: 4457
            },
            2
        )
    );
    assert_eq!(SLOTS, 15);
    // The last slot's ARC stays below the first flows port.
    assert!(slot(SLOTS - 1).0.arc < slot(0).0.flows);
}

/// Every one of the 15 slots is tried, and no 16th (`k < SLOTS`, exactly).
#[test]
fn the_last_slot_is_tried_and_none_after_it() {
    let tried = RefCell::new(Vec::new());
    let got = first_free(|p| {
        tried.borrow_mut().push(p.arc);
        p.arc == 4440 + SLOTS - 1
    });
    assert_eq!(got, Some(slot(14)));
    assert_eq!(*tried.borrow(), (4440..4455).collect::<Vec<u16>>());
    let tried = RefCell::new(0_u16);
    assert_eq!(
        first_free(|p| {
            *tried.borrow_mut() += 1;
            p.arc == 4455
        }),
        None
    );
    assert_eq!(*tried.borrow(), 15);
}

/// dasio's FOH: dlisten holds slot 0, band slot 1; dsend gets slot 2.
#[test]
fn the_first_slot_whose_ports_are_free_is_taken() {
    let taken = [4440, 8800, 4455, 4441, 8801, 4456];
    let got = first_free(|p| ![p.arc, p.cmc, p.flows].iter().any(|x| taken.contains(x)));
    assert_eq!(got, Some(slot(2)));
    // One port of a slot taken is enough to skip it.
    assert_eq!(first_free(|p| p.cmc != 8800), Some(slot(1)));
    assert_eq!(first_free(|p| p.flows != 4455), Some(slot(1)));
    assert_eq!(first_free(|_| false), None);
}

/// A fake bind refusing `held` (address and port); every other binds.
fn refusing(held: &[(Ipv4Addr, u16)]) -> impl Fn(SocketAddrV4) -> bool + '_ {
    move |at| !held.contains(&(*at.ip(), at.port()))
}

const OURS: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 10);
const ANY: Ipv4Addr = Ipv4Addr::UNSPECIFIED;

/// dstack #200: ConMon holds 0.0.0.0:8700 and 0.0.0.0:8800; a port held on
/// the wildcard address only skips the slot.
#[test]
fn a_port_held_on_the_wildcard_address_only_skips_the_slot() {
    let conmon = [(ANY, 8700), (ANY, 8800)];
    assert!(!binds_with(OURS, &slot(0).0, refusing(&conmon)));
    assert_eq!(choose(OURS, None, refusing(&conmon)), Some(slot(1)));
    for port in [4440, 8800, 4455] {
        assert!(
            !binds_with(OURS, &slot(0).0, refusing(&[(ANY, port)])),
            "{port}"
        );
    }
}

/// A port held on the device's own address skips the slot; another
/// address's holder does not.
#[test]
fn a_port_held_on_the_address_skips_the_slot() {
    assert_eq!(choose(OURS, None, refusing(&[])), Some(slot(0)));
    let first = [(OURS, 4440)];
    assert_eq!(choose(OURS, None, refusing(&first)), Some(slot(1)));
    for port in [4440, 8800, 4455] {
        assert!(
            !binds_with(OURS, &slot(0).0, refusing(&[(OURS, port)])),
            "{port}"
        );
    }
    let other = Ipv4Addr::new(192, 0, 2, 11);
    assert!(binds_with(OURS, &slot(0).0, refusing(&[(other, 8800)])));
}

/// The probe tries each port on the address, then on the wildcard address.
#[test]
fn each_port_is_probed_on_the_address_then_the_wildcard() {
    let seen = RefCell::new(Vec::new());
    assert!(binds_with(OURS, &slot(3).0, |at| {
        seen.borrow_mut().push((*at.ip(), at.port()));
        true
    }));
    assert_eq!(
        *seen.borrow(),
        vec![
            (OURS, 4443),
            (ANY, 4443),
            (OURS, 8803),
            (ANY, 8803),
            (OURS, 4458),
            (ANY, 4458)
        ]
    );
}

#[test]
fn no_slot_free_keeps_dantes_defaults() {
    assert_eq!(choose(OURS, None, |_| false), None);
    assert_eq!(effective(None), slot(0));
    assert_eq!(effective(Some(slot(4))), slot(4));
}

/// A test device's own slot wins: nothing is probed.
#[test]
fn given_ports_are_kept_and_nothing_is_probed() {
    let probed = RefCell::new(0);
    let got = choose(OURS, Some(slot(7)), |_| {
        *probed.borrow_mut() += 1;
        true
    });
    assert_eq!(got, Some(slot(7)));
    assert_eq!(*probed.borrow(), 0);
}

/// On the real sockets: a free port binds; one held on loopback is not free there.
#[test]
fn a_held_port_does_not_bind() {
    let held = std::net::UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = held.local_addr().unwrap().port();
    let free = std::net::UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
    let other = free.local_addr().unwrap().port();
    drop(free);
    let p = Ports {
        arc: other,
        cmc: other,
        flows: other,
    };
    assert!(binds(Ipv4Addr::LOCALHOST, &p));
    let p = Ports {
        arc: other,
        cmc: port,
        flows: other,
    };
    assert!(!binds(Ipv4Addr::LOCALHOST, &p));
}

#[test]
fn a_refusal_names_its_port() {
    assert_eq!(
        refused_port("ARC on UDP 4440: Address already in use (os error 98)"),
        Some(4440)
    );
    assert_eq!(refused_port("CMC on UDP 8801: denied"), Some(8801));
    assert_eq!(refused_port("the info server: denied"), None);
    assert_eq!(refused_port("ARC on UDP x: denied"), None);
}

#[test]
fn a_refusal_names_the_ports_holder_where_one_is_known() {
    let text = "ARC on UDP 4440: Address already in use";
    let asked = RefCell::new(Vec::new());
    let named = holder_text(text, |port| {
        asked.borrow_mut().push(port);
        Some((812, "Arena.exe".to_string()))
    });
    assert_eq!(
        named,
        "ARC on UDP 4440: Address already in use; UDP 4440 is held by Arena.exe (pid 812)"
    );
    assert_eq!(*asked.borrow(), vec![4440]);
    assert_eq!(holder_text(text, |_| None), text);
    assert_eq!(
        holder_text("no port here", |_| Some((1, "x.exe".into()))),
        "no port here"
    );
}

#[test]
fn the_holder_on_the_address_comes_before_the_wildcards() {
    let rows = [
        UdpRow {
            addr: ANY,
            port: 8800,
            pid: 4,
        },
        UdpRow {
            addr: OURS,
            port: 8800,
            pid: 9,
        },
        UdpRow {
            addr: Ipv4Addr::new(192, 0, 2, 11),
            port: 4440,
            pid: 5,
        },
    ];
    assert_eq!(holder_of(&rows, OURS, 8800), Some(9));
    assert_eq!(holder_of(&rows[..1], OURS, 8800), Some(4));
    assert_eq!(holder_of(&rows, OURS, 4440), None);
    assert_eq!(holder_of(&rows, OURS, 4455), None);
}

#[test]
fn a_table_rows_port_and_address_decode() {
    // 4440 = 0x1158, network order in the low 16 bits: bytes 0x11 0x58.
    assert_eq!(row_port(u32::from(u16::from_ne_bytes([0x11, 0x58]))), 4440);
    assert_eq!(
        row_port(0xABCD_0000 | u32::from(u16::from_ne_bytes([0x22, 0x60]))),
        8800
    );
    assert_eq!(
        row_addr(u32::from_ne_bytes([10, 77, 9, 201])),
        Ipv4Addr::new(10, 77, 9, 201)
    );
}

#[test]
fn an_image_path_reads_as_its_file_name() {
    assert_eq!(
        image_name(r"C:\Program Files\Resolume Arena\Arena.exe"),
        "Arena.exe"
    );
    assert_eq!(image_name("/usr/bin/conmon"), "conmon");
    assert_eq!(image_name("SongPlayer.exe"), "SongPlayer.exe");
}
