//! #233: the host's mapping of SongPlayer's stamps (`stamp.rs`).

use super::*;

const S: i64 = 10_000_000;
/// D(3) − D(1): two whole slots (D rounds each up: 1 000 000 − 333 334).
const TWO_SLOTS: i64 = 666_666;

/// The first block takes the offset of the two relabels at once; equal
/// relabels map a stamp to itself.
#[test]
fn the_first_block_takes_the_offset_of_the_two_relabels() {
    let mut m = StampMap::default();
    assert_eq!(m.offset_100ns(), None);
    assert_eq!(m.map(1_000, 3, 1, 0), 1_000 + TWO_SLOTS);
    assert_eq!(m.offset_100ns(), Some(TWO_SLOTS));
    assert_eq!(shift_100ns(3) - shift_100ns(1), TWO_SLOTS);
    let mut same = StampMap::default();
    assert_eq!(same.map(5_000, 2, 2, 0), 5_000);
    // A host started after SongPlayer followed a step: its K is 0.
    let mut fresh = StampMap::default();
    assert_eq!(fresh.map(7, 3, 0, 0), 7 + 1_000_000);
}

/// One side following a date step before the other (a split second) never
/// moves a stamp: the offset in force stays until a new one holds 1 s.
#[test]
fn a_new_offset_is_taken_only_once_it_held_a_second() {
    let mut m = StampMap::default();
    assert_eq!(m.map(0, 1, 1, 0), 0);
    // SongPlayer followed (K 1 → 3), the host not yet.
    assert_eq!(m.map(100, 3, 1, S), 100);
    assert_eq!(m.map(200, 3, 1, 2 * S - 1), 200, "held a tick under 1 s");
    assert_eq!(m.map(300, 3, 1, 2 * S), 300 + TWO_SLOTS, "held 1 s");
    assert_eq!(m.offset_100ns(), Some(TWO_SLOTS));
    // It stays.
    assert_eq!(m.map(400, 3, 1, 9 * S), 400 + TWO_SLOTS);
}

/// The host catching up within the second leaves the offset as it was.
#[test]
fn the_other_side_catching_up_cancels_the_change() {
    let mut m = StampMap::default();
    assert_eq!(m.map(0, 1, 1, 0), 0);
    assert_eq!(m.map(10, 3, 1, S), 10);
    // The host followed too: equal again, nothing pending.
    assert_eq!(m.map(20, 3, 3, S + S / 2), 20);
    // A later difference starts its own second.
    assert_eq!(m.map(30, 3, 1, 3 * S), 30);
    assert_eq!(m.map(40, 3, 1, 4 * S - 1), 40);
    assert_eq!(m.map(50, 3, 1, 4 * S), 50 + TWO_SLOTS);
}

/// A different offset seen meanwhile restarts the second: only the last one
/// that held a whole second is taken.
#[test]
fn a_second_different_offset_restarts_the_wait() {
    let mut m = StampMap::default();
    assert_eq!(m.map(0, 1, 1, 0), 0);
    // K 3 at 1 s, then K 4 at 1.9 s.
    assert_eq!(m.map(0, 3, 1, S), 0);
    assert_eq!(m.map(0, 4, 1, S + 9 * S / 10), 0);
    // 1.1 s after the first, 0.2 s after the second: nothing yet.
    assert_eq!(m.map(0, 4, 1, 2 * S + S / 10), 0);
    assert_eq!(m.map(0, 4, 1, 2 * S + 9 * S / 10 - 1), 0);
    let three = shift_100ns(4) - shift_100ns(1);
    assert_eq!(m.map(0, 4, 1, 2 * S + 9 * S / 10), three);
    assert_eq!(three, 1_000_000);
}
