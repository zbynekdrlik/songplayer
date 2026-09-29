//! #147: the WallClock re-anchor must not step the genlock wall. These tests
//! cover bracketed monotonic↔UTC anchor sampling and the bounded anchor update.
//!
//! The box relatched 6× in one A/V take. The unbracketed
//! `(Instant::now(), Utc::now())` anchor, preempted between its two reads,
//! jumped the wall FORWARD by the preemption time, and the next clean re-anchor
//! jumped it BACK. These tests drive a [`VirtualClock`] (a scriptable fake
//! source, no real sleep) and the pure helpers in `wallclock_anchor.rs`.
//! Wired via `#[cfg(test)] #[path = "wallclock_tests_anchor.rs"]` in
//! `wallclock.rs`.

use super::*;
use std::time::{Duration, Instant};

/// One 30-fps frame of virtual monotonic time (a multiple of 100 ns).
const FRAME_NS: u64 = 33_333_300;
/// A 40 ms preemption between the monotonic and the UTC read.
const PREEMPT_40MS_NS: u64 = 40_000_000;
/// 1 ms in 100-ns units.
const MS: i64 = 10_000;

/// Run 100 frames (one resample interval) and return the wall reading just
/// before and just after the resampling 100th tick.
fn resample(wall: &mut WallClock, clk: &VirtualClock) -> (i64, i64) {
    for _ in 0..99 {
        clk.advance_ns(FRAME_NS);
        wall.tick();
    }
    clk.advance_ns(FRAME_NS);
    let before = wall.now_100ns();
    wall.tick();
    assert_eq!(wall.frames_since_resample(), 0, "the 100th tick resamples");
    (before, wall.now_100ns())
}

/// Run one resample interval whose resampling 100th tick reads with
/// `delays` scripted: the step probe of every tick takes one read too (#224),
/// so a preemption meant for the resample is scripted right before its tick.
fn resample_reading(wall: &mut WallClock, clk: &VirtualClock, delays: &[u64]) -> (i64, i64) {
    for _ in 0..99 {
        clk.advance_ns(FRAME_NS);
        wall.tick();
    }
    clk.advance_ns(FRAME_NS);
    clk.delay_next_reads(delays);
    let before = wall.now_100ns();
    wall.tick();
    assert_eq!(wall.frames_since_resample(), 0, "the 100th tick resamples");
    (before, wall.now_100ns())
}

/// Advance one frame and tick; the wall just before and just after the tick.
fn tick_once(wall: &mut WallClock, clk: &VirtualClock) -> (i64, i64) {
    clk.advance_ns(FRAME_NS);
    let before = wall.now_100ns();
    wall.tick();
    (before, wall.now_100ns())
}

fn read(base: Instant, m1_us: u64, utc_100ns: i64, m2_us: u64) -> BracketedRead {
    BracketedRead {
        m1: base + Duration::from_micros(m1_us),
        utc_100ns,
        m2: base + Duration::from_micros(m2_us),
    }
}

/// Feed `reads` to `choose_bracketed_sample` in order. It panics if the
/// helper asks for more reads than scripted, and returns the sample and the
/// number of reads taken.
fn choose(reads: &[BracketedRead]) -> (AnchorSample, usize) {
    let mut i = 0;
    let s = choose_bracketed_sample(|| {
        let r = reads[i];
        i += 1;
        r
    });
    (s, i)
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

#[test]
fn choose_keeps_the_narrowest_of_eight_wide_reads_and_pairs_at_its_midpoint() {
    let b = Instant::now();
    let reads = [
        read(b, 0, 1, 40_000),
        read(b, 0, 2, 30_000),
        read(b, 0, 3, 1_000),
        read(b, 1_000, 4, 1_040), // 40 µs: the narrowest
        read(b, 0, 5, 5_000),
        read(b, 0, 6, 6_000),
        read(b, 0, 7, 7_000),
        read(b, 0, 8, 8_000),
    ];
    let (s, n) = choose(&reads);
    assert_eq!(
        n, ANCHOR_MAX_ATTEMPTS,
        "all 8 attempts are taken when every read is wide"
    );
    assert_eq!(n, 8);
    assert_eq!(s.utc_100ns, 4, "the narrowest bracket's UTC read");
    assert_eq!(s.bracket, Duration::from_micros(40));
    assert_eq!(
        s.instant,
        b + Duration::from_micros(1_020),
        "paired with the midpoint"
    );
    assert!(!s.is_wide());
}

#[test]
fn choose_stops_at_the_first_read_within_20_us() {
    let b = Instant::now();
    let reads = [read(b, 0, 11, 20), read(b, 0, 12, 0)];
    let (s, n) = choose(&reads);
    assert_eq!(n, 1, "a ≤ 20 µs bracket ends the sampling");
    assert_eq!(s.utc_100ns, 11);
    assert_eq!(s.instant, b + Duration::from_micros(10));

    // A preempted first read is re-tried and outvoted by the clean one.
    let reads = [read(b, 0, 21, 40_000), read(b, 50_000, 22, 50_000)];
    let (s, n) = choose(&reads);
    assert_eq!(n, 2);
    assert_eq!(s.utc_100ns, 22);
    assert_eq!(s.bracket, Duration::ZERO);
    assert_eq!(s.instant, b + Duration::from_micros(50_000));
}

#[test]
fn choose_keeps_the_earlier_read_on_a_tie() {
    let b = Instant::now();
    let mut reads = [read(b, 0, 31, 300); 8];
    // Every later read is distinct, so a `<=` pick (the last equally wide
    // read) cannot land on another 31 by accident.
    for (i, r) in reads.iter_mut().enumerate() {
        r.utc_100ns = 31 + i as i64;
    }
    let (s, n) = choose(&reads);
    assert_eq!(n, 8);
    assert_eq!(
        s.utc_100ns, 31,
        "an equally wide later read does not replace the first"
    );
}

#[test]
fn a_bracket_is_wide_only_strictly_above_200_us() {
    let b = Instant::now();
    let (at_200, _) = choose(&[read(b, 0, 1, 200); 8]);
    assert_eq!(at_200.bracket, ANCHOR_WIDE_BRACKET);
    assert!(!at_200.is_wide(), "exactly 200 µs is not wide");
    let (at_201, _) = choose(&[read(b, 0, 1, 201); 8]);
    assert!(at_201.is_wide(), "201 µs is wide");
}

#[test]
fn bounded_update_applies_within_1_ms_and_clamps_beyond() {
    let s = bounded_anchor_update(3_133);
    assert_eq!((s.applied_100ns, s.carry_100ns), (3_133, 0));
    assert!(!s.is_clamped());
    let s = bounded_anchor_update(-MS);
    assert_eq!(
        (s.applied_100ns, s.carry_100ns),
        (-MS, 0),
        "exactly 1 ms applies"
    );
    assert!(!s.is_clamped());
    let s = bounded_anchor_update(50 * MS);
    assert_eq!((s.applied_100ns, s.carry_100ns), (MS, 49 * MS));
    assert!(s.is_clamped());
    let s = bounded_anchor_update(-40 * MS - 7);
    assert_eq!((s.applied_100ns, s.carry_100ns), (-MS, -39 * MS - 7));
    assert!(s.is_clamped());
    assert_eq!(ANCHOR_MAX_STEP_100NS, MS);
}

#[test]
fn apply_step_moves_forward_or_holds_never_backward() {
    let at = Instant::now();
    assert_eq!(apply_anchor_step(at, 1_000_000, 2_500), (at, 1_002_500));
    assert_eq!(apply_anchor_step(at, 1_000_000, 0), (at, 1_000_000));
    // Backward: same wall value, anchored 250 µs later, so it holds, then
    // runs 250 µs behind the old line.
    let (inst, utc) = apply_anchor_step(at, 1_000_000, -2_500);
    assert_eq!(inst, at + Duration::from_micros(250));
    assert_eq!(utc, 1_000_000);
    assert_eq!(
        wall_at(inst, utc, at),
        1_000_000,
        "held at the sample instant"
    );
    assert_eq!(
        wall_at(inst, utc, at + Duration::from_micros(100)),
        1_000_000
    );
    assert_eq!(
        wall_at(inst, utc, at + Duration::from_millis(1)),
        1_000_000 + 7_500,
        "then on the corrected line: old line − 250 µs"
    );
}

#[test]
fn wall_at_is_anchor_plus_elapsed_saturating_before_the_anchor() {
    let a = Instant::now() + Duration::from_millis(5);
    assert_eq!(wall_at(a, 42, a + Duration::from_millis(1)), 42 + MS);
    assert_eq!(wall_at(a, 42, a + Duration::from_nanos(250)), 44);
    assert_eq!(wall_at(a, 42, a - Duration::from_millis(1)), 42);
}

#[test]
fn to_us_converts_100ns_units() {
    assert_eq!(to_us(123_456), 12_345);
    assert_eq!(to_us(-50 * MS), -50_000);
}

#[test]
fn anchor_stats_record_max_delta_wide_brackets_and_slewed_total() {
    let b = Instant::now();
    let mut st = WallAnchorStats::default();
    st.record_sample(&choose(&[read(b, 0, 1, 400); 8]).0);
    st.record_sample(&choose(&[read(b, 0, 1, 200); 8]).0);
    st.record_step(-2_000, &bounded_anchor_update(-2_000));
    st.record_step(55_000, &bounded_anchor_update(55_000));
    st.record_step(-45_000, &bounded_anchor_update(-45_000));
    assert_eq!(
        st,
        WallAnchorStats {
            max_step_us: 5_500,
            wide_brackets: 1,
            slewed_us: 2_000,
            steps_followed: 0,
            last_step_us: 0,
            holds_followed: 0,
            last_hold_us: 0,
        }
    );
}

// ---------------------------------------------------------------------------
// WallClock over the virtual clock (#147 acceptance)
// ---------------------------------------------------------------------------

#[test]
fn a_40_ms_delayed_utc_read_yields_an_anchor_within_200_us_of_truth() {
    let clk = VirtualClock::new(0);
    clk.delay_next_reads(&[PREEMPT_40MS_NS]);
    let wall = WallClock::new(Box::new(clk.clone()));
    let err = wall.now_100ns() - clk.truth_100ns();
    assert!(
        err.abs() <= 2_000,
        "the anchor must come from the clean attempt, off truth by {err} × 100 ns"
    );
    assert_eq!(clk.reads(), 2, "the preempted read is re-tried once");
    assert_eq!(wall.anchor_stats().wide_brackets, 0);
}

#[test]
fn a_delayed_pairing_then_a_clean_one_never_moves_the_wall_back_or_over_1_ms() {
    let clk = VirtualClock::new(0);
    let mut wall = WallClock::new(Box::new(clk.clone()));
    let mut prev = wall.now_100ns();
    for frame in 1..=1_000u64 {
        clk.advance_ns(FRAME_NS);
        // Resamples 1, 3, 5, … (frames 100, 300, …) meet a 40 ms preemption.
        // The even ones are clean.
        if frame % 200 == 100 {
            clk.delay_next_reads(&[PREEMPT_40MS_NS]);
        }
        let before = wall.now_100ns();
        assert!(
            before >= prev,
            "frame {frame}: wall went back between frames"
        );
        wall.tick();
        let after = wall.now_100ns();
        assert!(
            after >= before,
            "frame {frame}: the re-anchor stepped the wall back"
        );
        assert!(
            after - before <= ANCHOR_MAX_STEP_100NS,
            "frame {frame}: the re-anchor stepped the wall by {} × 100 ns",
            after - before
        );
        assert_eq!(
            after,
            clk.truth_100ns(),
            "frame {frame}: wall left the truth"
        );
        prev = after;
    }
    assert_eq!(wall.anchor_stats(), WallAnchorStats::default());
}

#[test]
fn every_attempt_preempted_still_moves_at_most_1_ms_and_never_back() {
    let clk = VirtualClock::new(0);
    let mut wall = WallClock::new(Box::new(clk.clone()));
    let mut prev = wall.now_100ns();
    for frame in 1..=1_000u64 {
        clk.advance_ns(FRAME_NS);
        // Resample 1 (frame 100): all 8 attempts are preempted, so the best
        // pairing is still 20 ms off. The bound lets it move the wall by 1 ms
        // only. Resample 2 (clean) takes that 1 ms back as a hold.
        if frame == 100 {
            clk.delay_next_reads(&[PREEMPT_40MS_NS; 8]);
        }
        let before = wall.now_100ns();
        assert!(
            before >= prev,
            "frame {frame}: wall went back between frames"
        );
        wall.tick();
        let after = wall.now_100ns();
        assert!(
            after >= before,
            "frame {frame}: the re-anchor stepped the wall back"
        );
        assert!(
            after - before <= ANCHOR_MAX_STEP_100NS,
            "frame {frame}: step over 1 ms"
        );
        prev = after;
    }
    assert_eq!(
        wall.now_100ns(),
        clk.truth_100ns(),
        "back on truth after the hold"
    );
    assert_eq!(
        wall.anchor_stats(),
        WallAnchorStats {
            max_step_us: 20_000,
            wide_brackets: 1,
            slewed_us: 1_000,
            steps_followed: 0,
            last_step_us: 0,
            holds_followed: 0,
            last_hold_us: 0,
        }
    );
}

#[test]
fn a_genuine_plus_50_ms_utc_step_is_followed_in_one_event_at_the_next_boundary() {
    // #224 (design record 5890605448): a dantesync fleet date step (+50 ms,
    // forward) is followed at the boundary the step probe first sees it, in
    // ONE event, as every fleet receiver sees it at once — no longer 1 ms,
    // then the rest two resamples (3.3–6.7 s) later (#147).
    let clk = VirtualClock::new(0);
    let mut wall = WallClock::new(Box::new(clk.clone()));
    clk.step_utc(50 * MS);
    let (before, after) = tick_once(&mut wall, &clk);
    assert_eq!(
        wall.frames_since_resample(),
        0,
        "the follow restarts the count"
    );
    assert_eq!(after - before, 50 * MS, "the whole step in one event");
    assert_eq!(after, clk.truth_100ns(), "on the stepped UTC at once");
    // The next resample, 100 ticks after the follow, is normal again.
    for _ in 0..99 {
        tick_once(&mut wall, &clk);
    }
    let (before, after) = tick_once(&mut wall, &clk);
    assert_eq!(wall.frames_since_resample(), 0, "the 100th tick resampled");
    assert_eq!(after, before);
    assert_eq!(after, clk.truth_100ns());
    assert_eq!(
        wall.anchor_stats(),
        WallAnchorStats {
            max_step_us: 50_000,
            wide_brackets: 0,
            slewed_us: 0,
            steps_followed: 1,
            last_step_us: 50_000,
            holds_followed: 0,
            last_hold_us: 0,
        }
    );
}

#[test]
fn a_genuine_minus_50_ms_utc_step_is_one_hold_at_the_next_boundary_never_stepped_back() {
    // #224: a backward date step is followed at the boundary the probe first
    // sees it, as ONE hold of the whole step (#147: the wall freezes, then
    // runs on the UTC line; it is never stepped back).
    let clk = VirtualClock::new(0);
    let mut wall = WallClock::new(Box::new(clk.clone()));
    clk.step_utc(-50 * MS);
    let (before, after) = tick_once(&mut wall, &clk);
    assert_eq!(after, before, "ONE hold, never a step back");
    clk.advance_ns(25_000_000);
    assert_eq!(wall.now_100ns(), before, "still holding half-way");
    clk.advance_ns(25_000_000);
    assert_eq!(wall.now_100ns(), before, "held for exactly 50 ms");
    assert_eq!(
        wall.now_100ns(),
        clk.truth_100ns(),
        "the UTC line reached it"
    );
    clk.advance_ns(1_000_000);
    assert_eq!(
        wall.now_100ns(),
        clk.truth_100ns(),
        "then it runs on the line"
    );
    // The next resample, 100 ticks after the follow, is normal again.
    for _ in 0..99 {
        tick_once(&mut wall, &clk);
    }
    let (before, after) = tick_once(&mut wall, &clk);
    assert_eq!(wall.frames_since_resample(), 0, "the 100th tick resampled");
    assert_eq!(after, before);
    assert_eq!(after, clk.truth_100ns());
    assert_eq!(
        wall.anchor_stats(),
        WallAnchorStats {
            max_step_us: 50_000,
            wide_brackets: 0,
            slewed_us: 0,
            steps_followed: 1,
            last_step_us: -50_000,
            holds_followed: 1,
            last_hold_us: 50_000,
        }
    );
}

#[test]
fn a_94_ppm_dantesync_slew_is_followed_exactly_either_way() {
    for ppm in [94i64, -94] {
        let clk = VirtualClock::new(ppm);
        let mut wall = WallClock::new(Box::new(clk.clone()));
        for r in 1..=30 {
            let (before, after) = resample(&mut wall, &clk);
            assert!(after >= before, "ppm {ppm} resample {r}: stepped back");
            assert!(
                after - before <= MS,
                "ppm {ppm} resample {r}: step over 1 ms"
            );
            // 1 ms later (past any hold) the wall is ON the slewed truth.
            clk.advance_ns(1_000_000);
            let err = wall.now_100ns() - clk.truth_100ns();
            assert!(
                err.abs() <= 2,
                "ppm {ppm} resample {r}: off truth by {err} × 100 ns"
            );
        }
        let st = wall.anchor_stats();
        assert!(
            (310..=320).contains(&st.max_step_us),
            "ppm {ppm}: ~313 µs per 3.33 s resample, got {}",
            st.max_step_us
        );
        assert_eq!(st.slewed_us, 0, "ppm {ppm}: a 94 ppm slew is never clamped");
        assert_eq!(st.wide_brackets, 0);
    }
}

#[test]
fn the_counters_report_max_step_wide_brackets_and_slewed_total() {
    let clk = VirtualClock::new(0);
    // Construction: 8 preempted attempts, the narrowest 400 µs. The anchor is
    // 200 µs ahead of truth and counts one wide bracket.
    clk.delay_next_reads(&[
        40_000_000, 30_000_000, 1_000_000, 400_000, 5_000_000, 6_000_000, 7_000_000, 8_000_000,
    ]);
    let mut wall = WallClock::new(Box::new(clk.clone()));
    assert_eq!(clk.reads(), 8);
    assert_eq!(wall.now_100ns() - clk.truth_100ns(), 2_000);
    assert_eq!(wall.anchor_stats().wide_brackets, 1);

    // A clean resample measures −200 µs and holds it out.
    resample(&mut wall, &clk);
    assert_eq!(wall.anchor_stats().max_step_us, 200);
    clk.advance_ns(FRAME_NS);
    assert_eq!(wall.now_100ns(), clk.truth_100ns());

    // Eight 200 µs brackets on the RESAMPLE's reads: not wide (strictly wider
    // only), the 100 µs midpoint error applies as-is.
    resample_reading(&mut wall, &clk, &[200_000; 8]);
    assert_eq!(wall.anchor_stats().wide_brackets, 1);
    assert_eq!(wall.now_100ns() - clk.truth_100ns(), 1_000);

    // A +2 ms UTC step, 1.9 ms from the wall: within the step probe's 2 ms
    // (#224), so the resample slews it — 1 ms applied.
    clk.step_utc(2 * MS);
    let (before, after) = resample(&mut wall, &clk);
    assert_eq!(after - before, MS);
    // A wide RESAMPLE counts too: 400 µs brackets, delta 1.1 ms, 1 ms more
    // slewed, the 100 µs midpoint error left.
    resample_reading(&mut wall, &clk, &[400_000; 8]);
    assert_eq!(wall.now_100ns() - clk.truth_100ns(), 1_000);
    // The anchor reads (8 + 1 + 8 + 1 + 8), plus one step-probe read per tick.
    assert_eq!(clk.reads(), 8 + 1 + 8 + 1 + 8 + 4 * 100);
    assert_eq!(
        wall.anchor_stats(),
        WallAnchorStats {
            max_step_us: 1_900,
            wide_brackets: 2,
            slewed_us: 2_000,
            steps_followed: 0,
            last_step_us: 0,
            holds_followed: 0,
            last_hold_us: 0,
        }
    );
}
