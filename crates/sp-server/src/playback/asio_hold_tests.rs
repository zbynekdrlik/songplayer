//! #233 review round 1: one holder per ASIO driver in the process.

use super::*;

const DVS: &str = "Dante Virtual Soundcard (x64)";
const BLACKMAGIC: &str = "Blackmagic ASIO";

#[test]
fn a_held_driver_is_refused_until_its_hold_is_dropped() {
    let holds = DriverHolds::new();
    let first = holds.claim(DVS);
    assert!(first.is_some(), "a free driver is held");
    assert!(holds.is_held(DVS));
    assert!(holds.claim(DVS).is_none(), "a held driver is refused");
    drop(first);
    assert!(!holds.is_held(DVS), "a dropped hold gives the driver back");
    assert!(holds.claim(DVS).is_some(), "and it can be held again");
}

#[test]
fn each_driver_is_held_on_its_own() {
    let holds = DriverHolds::default();
    let dvs = holds.claim(DVS).expect("free");
    let blackmagic = holds.claim(BLACKMAGIC).expect("another driver is free");
    drop(dvs);
    assert!(!holds.is_held(DVS));
    assert!(
        holds.is_held(BLACKMAGIC),
        "dropping one hold gives back only its own driver"
    );
    drop(blackmagic);
    assert!(!holds.is_held(BLACKMAGIC));
}

/// #233 review round 3: a parked driver is never given back, and reads as
/// parked (a held one does not).
#[test]
fn a_parked_driver_stays_held_and_reads_parked() {
    let holds = DriverHolds::new();
    holds.claim(DVS).expect("free").park();
    assert!(holds.is_held(DVS), "a parked driver is never given back");
    assert!(holds.is_parked(DVS));
    assert!(holds.claim(DVS).is_none());
    let other = holds.claim(BLACKMAGIC).expect("free");
    assert!(!holds.is_parked(BLACKMAGIC), "held is not parked");
    drop(other);
    assert!(!holds.is_parked(BLACKMAGIC));
}
