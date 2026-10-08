//! #233 (the main session's ruling, comment 6056680979, Q1): the excess an
//! underrun leaves is KEPT as cushion over the target, at most one slot; the
//! offset slew drains only what lies outside [target, target + cushion];
//! the level loop keeps the configured target, so the cushion decays through
//! it, and follows the window mean down. Pins from a scratch model of the
//! servo.

use super::*;

const RATE: f64 = 96_000.0;
const T0: i64 = 17_900_000_000_000_000;

/// Block `k` handled at its boundary with `buffered` frames waiting, the card
/// at `consumed`, `underruns` frames of silence played so far.
fn obs(k: i64, buffered: u64, consumed: u64, underruns: u64) -> Observation {
    Observation {
        handled_100ns: T0 + k * SLOT_100NS,
        stamp_100ns: T0 + k * SLOT_100NS,
        buffered_frames: buffered,
        pending_skip_frames: 0,
        consumed_frames: consumed,
        underrun_frames: underruns,
    }
}

/// Blocks `from..from + n` with `buffered` frames waiting, the card at the
/// nominal rate, `underruns` frames so far; the last action.
fn blocks(s: &mut Servo, from: i64, n: i64, buffered: u64, underruns: u64) -> ServoAction {
    let mut last = None;
    for k in from..from + n {
        let consumed = ((k * SLOT_100NS) as f64 / 1e7 * RATE) as u64;
        last = Some(s.observe(obs(k, buffered, consumed, underruns)));
    }
    last.expect("at least one block")
}

#[test]
fn an_underruns_excess_is_added_up_to_one_slot() {
    assert_eq!(fold_cushion(0, 133_333), 133_333);
    assert_eq!(fold_cushion(133_333, 199_999), 333_332);
    assert_eq!(fold_cushion(133_333, 200_000), 333_333, "one slot exactly");
    assert_eq!(fold_cushion(133_333, 200_001), 333_333);
    assert_eq!(fold_cushion(i64::MAX - 5, 10), 333_333, "saturating");
}

#[test]
fn the_cushion_follows_the_window_mean_down_never_up() {
    let t = BASE_LATENCY_100NS;
    assert_eq!(kept_cushion(200_000, t + 150_000, t), 150_000);
    assert_eq!(kept_cushion(200_000, t + 250_000, t), 200_000, "never up");
    assert_eq!(kept_cushion(200_000, t, t), 0);
    assert_eq!(kept_cushion(200_000, t - 5, t), 0, "never negative");
}

/// The slew's error is the latency's distance from the band [target,
/// target + cushion]: a deficit from the target, an excess from the top,
/// nothing inside.
#[test]
fn the_slew_drains_only_what_lies_outside_the_cushions_band() {
    let t = BASE_LATENCY_100NS;
    assert_eq!(slew_err_100ns(t - 50, t, 200_000), 50);
    assert_eq!(slew_err_100ns(t, t, 200_000), 0);
    assert_eq!(slew_err_100ns(t + 199_999, t, 200_000), 0);
    assert_eq!(slew_err_100ns(t + 200_000, t, 200_000), 0);
    assert_eq!(slew_err_100ns(t + 200_001, t, 200_000), -1);
    assert_eq!(slew_err_100ns(t + 7, t, 0), -7, "no cushion: the target");
    assert_eq!(slew_err_100ns(t - 7, t, 0), 7);
}

/// The card's underrun frames (cumulative) since the last block are folded
/// in: 1 280 frames are 13.3 ms; a count that went back (a new count) folds
/// nothing; more than a slot in all is capped at one slot; a hard re-centre
/// (the latency under the floor) drops it. The first block's count is the
/// start, never folded.
#[test]
fn underrun_frames_fold_into_a_cushion_that_a_re_centre_drops() {
    let mut s = Servo::new(RATE, BASE_LATENCY_100NS);
    s.observe(obs(0, 6_400, 0, 500));
    s.observe(obs(1, 6_400, 3_200, 500));
    assert_eq!(s.status().cushion_ms, 0.0, "the start is no underrun");
    s.observe(obs(2, 6_400, 6_400, 1_780));
    assert_eq!(s.status().cushion_ms, 13.3333);
    s.observe(obs(3, 6_400, 9_600, 1_000));
    assert_eq!(s.status().cushion_ms, 13.3333, "a count that went back");
    s.observe(obs(4, 6_400, 12_800, 1_908));
    assert_eq!(
        s.status().cushion_ms,
        14.6666,
        "128 frames past the most seen"
    );
    s.observe(obs(5, 6_400, 16_000, 5_748));
    assert_eq!(s.status().cushion_ms, 33.3333, "one slot at most");
    let a = s.observe(obs(6, 0, 19_200, 5_748));
    assert_eq!(a.recentre, Some(Recentre::Deficit));
    assert_eq!(s.status().cushion_ms, 0.0, "the re-centre drops it");
}

/// The ruling's case: an underrun left 20 ms (1 921 frames counted, a whole
/// callback for the short one) and the latency stands that much over the
/// target. It is kept, not braked: no offset, no time left, the correction
/// the level loop's alone (the stop curve would ask −300 ppm and slew to
/// −5.17 in the first window). The window that folded keeps the counted
/// 20.0104 ms; the next one cuts it to what it held, 20.0001 ms; it follows
/// the latency down (10.0001 ms) and is gone once the latency is under the
/// target, which the slew drains again.
#[test]
fn a_kept_cushion_is_not_braked_and_follows_the_latency_down() {
    let mut s = Servo::new(RATE, BASE_LATENCY_100NS);
    s.observe(obs(0, 6_400, 0, 0));
    let w1 = blocks(&mut s, 1, 32, 8_320, 1_921);
    assert!(
        (w1.correction_ppm - -3.750372250553069).abs() < 1e-9,
        "{w1:?}"
    );
    let st = s.status();
    assert_eq!(
        (st.cushion_ms, st.offset_ms, st.slew_eta_s),
        (20.0104, 0.0, None),
        "{st:?}"
    );
    let w2 = blocks(&mut s, 33, 32, 8_320, 1_921);
    assert!(
        (w2.correction_ppm - -7.248993473053323).abs() < 1e-9,
        "{w2:?}"
    );
    assert_eq!(s.status().cushion_ms, 20.0001);
    let w3 = blocks(&mut s, 65, 32, 7_360, 1_921);
    assert!(
        (w3.correction_ppm - -8.480967310481146).abs() < 1e-9,
        "{w3:?}"
    );
    assert_eq!(s.status().cushion_ms, 10.0001);
    let w4 = blocks(&mut s, 97, 32, 6_000, 1_921);
    assert!(
        (w4.correction_ppm - -3.1476393104811455).abs() < 1e-9,
        "{w4:?}"
    );
    let st = s.status();
    assert_eq!((st.cushion_ms, st.offset_ms), (0.0, -4.1666), "{st:?}");
    assert!(st.slew_eta_s.is_some(), "a deficit is slewed: {st:?}");
    assert_eq!(st.hard_recentres, 0);
}
