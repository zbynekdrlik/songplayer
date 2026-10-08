//! #233: the drift servo — its constants are camera-box's, the regression's
//! lock / eviction / re-base / restart edges, the level loop's clamps and
//! anti-windup, the slew; the offset slew's stop curve and its time left;
//! then `Servo::observe`: the first block's priming (no re-centre), the hard
//! re-centres' 50 ms / 133.3 ms edges, a window mean off target slewed
//! never spliced, the window's 1 s and starvation edges, a fast card, the
//! slew, a starved window, a jump of the card's position. Pins derived with a
//! scratch model of this file (two independent runs agree).

use super::*;

#[test]
fn the_constants_are_camera_boxs() {
    // camera-box src/asrc_bench.rs (MIT) and vendor/obs-studio/libobs/media-io/asrc-compensator.h
    assert_eq!(MAX_PPM, 300.0); // :204 / .h:42
    assert_eq!(MAX_SLEW_PPM_PER_S, 5.0); // :209 / .h:48
    assert_eq!(REGRESSION_SPAN_S, 600.0); // :223 / .h:62
    assert_eq!(REGRESSION_MIN_POINTS, 30); // :228 / .h:67
    assert_eq!(REGRESSION_LOCK_SPAN_S, 60.0); // :235 / .h:76
    assert_eq!(REGRESSION_CAP, 640); // :243 / .h:82
    assert_eq!(WINDOW_100NS, 10_000_000); // WINDOW_S 1.0, :470 / .h:120
    assert_eq!(STEP_RESIDUAL_S, 0.010); // STEP_RESIDUAL_MS 10, :284 / .h:149
    assert_eq!(LEVEL_KP_PPM_PER_MS, 2.0); // :373 / .h:232
    assert_eq!(LEVEL_KP_MAX_PPM, 50.0); // :380 / .h:239
    assert_eq!(LEVEL_EMA_TAU_S, 10.0); // :392 / .h:251
    assert_eq!(LEVEL_KI_PPM_PER_MS_S, 0.0002); // :264 / .h:130
    assert_eq!(LEVEL_INTEGRAL_MAX_PPM, 3.0); // :271 / .h:137
    assert_eq!(MAX_SANE_WINDOW_PPM, 100_000.0); // :447 / .h:109
    // SongPlayer's own: one grid slot, VBAN's send budget, the last
    // resort's edges, the calm zone.
    assert_eq!(
        SLOT_100NS,
        sp_core::genlock::UNITS_PER_SECOND / sp_core::genlock::GENLOCK_GRID_FPS
    );
    assert_eq!(
        BASE_LATENCY_100NS,
        crate::playback::vban_packet::VBAN_SEND_LATENCY_100NS
    );
    assert_eq!(HARD_DEFICIT_100NS, 500_000, "50 ms, 1.5 slots");
    assert_eq!(HARD_EXCESS_100NS, 4 * SLOT_100NS + 1, "four slots");
    assert_eq!(CALM_ZONE_MS, 1.0);
}

/// `n` points `step_s` apart from `x0` on y = ppm·1e-6·x + 0.5.
fn line_from(r: &mut RateRegression, x0: f64, n: usize, step_s: f64, ppm: f64) {
    for i in 0..n {
        let x = x0 + i as f64 * step_s;
        assert_eq!(
            r.offer(x, ppm * 1e-6 * x + 0.5),
            Offered::Inserted,
            "point {i}"
        );
    }
}

fn line(r: &mut RateRegression, n: usize, step_s: f64, ppm: f64) {
    line_from(r, 0.0, n, step_s, ppm);
}

#[test]
fn the_rate_locks_at_30_points_spanning_60_s() {
    let mut r = RateRegression::default();
    line(&mut r, 30, 2.0, 20.0); // 30 points over 58 s
    assert!(!r.locked());
    assert_eq!(r.rate_ppm(), 0.0, "no rate before the lock");
    let mut r = RateRegression::default();
    line(&mut r, 29, 3.0, 20.0); // 29 points over 84 s
    assert!(!r.locked());
    let mut r = RateRegression::default();
    line(&mut r, 31, 2.0, 20.0); // 31 points over exactly 60 s
    assert!(r.locked());
    assert!((r.rate_ppm() - 20.0).abs() < 1e-6, "{}", r.rate_ppm());
}

/// The span is the newest point less the oldest, wherever the line starts.
#[test]
fn the_lock_span_is_measured_from_the_oldest_point() {
    let mut r = RateRegression::default();
    line_from(&mut r, 50.0, 30, 2.0, 20.0); // 50 → 108 s
    assert!(!r.locked(), "58 s");
    let mut r = RateRegression::default();
    line_from(&mut r, 50.0, 31, 2.0, 20.0); // 50 → 110 s
    assert!(r.locked(), "60 s");
    assert!((r.rate_ppm() - 20.0).abs() < 1e-6, "{}", r.rate_ppm());
}

#[test]
fn exactly_30_points_over_exactly_60_s_lock() {
    let mut r = RateRegression::default();
    for i in 0..29 {
        assert_eq!(r.offer(f64::from(i * 2), 0.0), Offered::Inserted);
    }
    assert_eq!(r.offer(60.0, 0.0), Offered::Inserted);
    assert_eq!(r.point_count(), 30);
    assert!(r.locked());
}

#[test]
fn old_points_leave_by_span_and_by_cap() {
    let mut r = RateRegression::default();
    line(&mut r, 700, 1.0, -30.0);
    assert_eq!(r.point_count(), 601, "600 s of 1 s points");
    let mut r = RateRegression::default();
    line(&mut r, 700, 0.5, -30.0);
    assert_eq!(r.point_count(), REGRESSION_CAP);
    assert!((r.rate_ppm() + 30.0).abs() < 1e-6);
}

#[test]
fn a_step_after_the_lock_rebases_and_keeps_the_rate() {
    let mut r = RateRegression::default();
    line(&mut r, 100, 1.0, 20.0);
    let rate = r.rate_ppm();
    assert_eq!(
        r.offer(100.0, 20e-6 * 100.0 + 0.5 + 0.044),
        Offered::Rebased
    );
    assert_eq!(
        r.offer(101.0, 20e-6 * 101.0 + 0.5 + 0.044),
        Offered::Realigned,
        "the next point realigns onto the moved line"
    );
    assert_eq!(
        r.offer(102.0, 20e-6 * 102.0 + 0.5 + 0.044),
        Offered::Inserted,
        "the new line is the old one"
    );
    assert!(
        (r.rate_ppm() - rate).abs() < 1e-6,
        "the rate is not disturbed"
    );
}

/// A 30 ms step inside a window: its mean is 22 ms off (re-based), the next
/// window's is 30 ms off — 8 ms from the moved line, under the step residual,
/// so without the realign it would stay in the regression as a level shift.
#[test]
fn a_step_straddling_a_window_realigns_the_next_point() {
    let mut r = RateRegression::default();
    line(&mut r, 100, 1.0, 20.0);
    let rate = r.rate_ppm();
    let on = |x: f64| 20e-6 * x + 0.5;
    assert_eq!(r.offer(100.0, on(100.0) + 0.022), Offered::Rebased);
    assert_eq!(r.offer(101.0, on(101.0) + 0.030), Offered::Realigned);
    assert_eq!(r.offer(102.0, on(102.0) + 0.030), Offered::Inserted);
    assert_eq!(r.offer(103.0, on(103.0) + 0.030), Offered::Inserted);
    assert!(
        (r.rate_ppm() - rate).abs() < 1e-6,
        "{} vs {rate}",
        r.rate_ppm()
    );
    // A 44 ms step: 22 ms in the straddled window's mean, 22 more in the
    // next — the rest of the same step realigns (one step, one rebase).
    let mut r = RateRegression::default();
    line(&mut r, 100, 1.0, 20.0);
    assert_eq!(r.offer(100.0, on(100.0) + 0.022), Offered::Rebased);
    assert_eq!(r.offer(101.0, on(101.0) + 0.044), Offered::Realigned);
    assert_eq!(r.offer(102.0, on(102.0) + 0.044), Offered::Inserted);
    // A flush forgets a pending realign.
    let mut r = RateRegression::default();
    line(&mut r, 100, 1.0, 20.0);
    assert_eq!(r.offer(100.0, on(100.0) + 0.022), Offered::Rebased);
    r.flush();
    line(&mut r, 31, 1.0, 20.0);
    assert_eq!(r.offer(31.0, on(31.0) + 0.005), Offered::Inserted);
}

/// A steep line (1000 ppm) from x = 50 s: every point on it is a point, one
/// 11 ms above it is a step, the next one realigns, then the moved line goes on.
#[test]
fn a_steep_line_is_fitted_through_its_points() {
    let mut r = RateRegression::default();
    line_from(&mut r, 50.0, 70, 1.0, 1000.0);
    assert!(r.locked());
    assert!((r.rate_ppm() - 1000.0).abs() < 1e-6, "{}", r.rate_ppm());
    assert_eq!(r.offer(120.0, 1e-3 * 120.0 + 0.5), Offered::Inserted);
    assert_eq!(r.offer(121.0, 1e-3 * 121.0 + 0.5 + 0.011), Offered::Rebased);
    assert_eq!(
        r.offer(122.0, 1e-3 * 122.0 + 0.5 + 0.011),
        Offered::Realigned
    );
    assert_eq!(
        r.offer(123.0, 1e-3 * 123.0 + 0.5 + 0.011),
        Offered::Inserted
    );
}

/// `n` points one second apart on y = 0 (exact in f64, so a residual of
/// exactly `STEP_RESIDUAL_S` can be pinned; `0.5 + 0.010 − 0.5` is not 0.010).
fn flat(r: &mut RateRegression, n: u32) {
    for i in 0..n {
        assert_eq!(r.offer(f64::from(i), 0.0), Offered::Inserted);
    }
}

#[test]
fn a_residual_of_exactly_10_ms_is_a_point_and_more_is_a_step() {
    let mut r = RateRegression::default();
    flat(&mut r, 40);
    assert_eq!(r.offer(40.0, 0.010), Offered::Inserted);
    let mut r = RateRegression::default();
    flat(&mut r, 40);
    assert_eq!(r.offer(40.0, 0.0101), Offered::Rebased);
}

/// The fit takes over from the 31st point (30 behind it), not later.
#[test]
fn the_fit_judges_a_step_once_it_has_30_points() {
    let mut r = RateRegression::default();
    flat(&mut r, 30);
    assert_eq!(r.offer(30.0, 0.0101), Offered::Rebased, "the 31st");
    let mut r = RateRegression::default();
    flat(&mut r, 29);
    assert_eq!(r.offer(29.0, 0.0101), Offered::Restarted, "the 30th");
}

#[test]
fn a_jump_before_the_lock_restarts_the_regression() {
    let mut r = RateRegression::default();
    flat(&mut r, 5);
    assert_eq!(r.offer(5.0, 0.020), Offered::Restarted);
    assert_eq!(r.point_count(), 1);
    assert_eq!(
        r.offer(6.0, 0.020),
        Offered::Inserted,
        "the line goes on from the jumped point"
    );
    assert_eq!(r.point_count(), 2);
    let mut r = RateRegression::default();
    flat(&mut r, 5);
    assert_eq!(
        r.offer(5.0, 0.010),
        Offered::Inserted,
        "10 ms is still a point"
    );
    r.flush();
    assert_eq!(r.point_count(), 0);
}

#[test]
fn points_at_one_instant_give_no_slope() {
    let mut r = RateRegression::default();
    for _ in 0..40 {
        r.offer(5.0, 0.5);
    }
    assert_eq!(r.rate_ppm(), 0.0);
    assert_eq!(r.fit(), None);
}

#[test]
fn the_level_loop_p_is_a_smoothed_clamped_2_ppm_per_ms() {
    let mut l = LevelLoop::default();
    let out = l.update(10.0, 1.0, 0.0);
    let alpha = 1.0 / 11.0;
    assert!((l.ema_ms() - 10.0 * alpha).abs() < 1e-12);
    assert!(
        (out - (2.0 * 10.0 * alpha + 0.0002 * 10.0)).abs() < 1e-12,
        "P + I"
    );
    l.update(10.0, 1.0, 0.0);
    assert!(
        (l.ema_ms() - 10.0 * alpha * (2.0 - alpha)).abs() < 1e-12,
        "the second window moves the EMA toward the error"
    );
    let mut l = LevelLoop::default();
    for _ in 0..200 {
        l.update(40.0, 1.0, 0.0);
    }
    assert!(
        (l.update(40.0, 1.0, 0.0) - (50.0 + 1.6)).abs() < 0.1,
        "P at its 50 ppm clamp, I growing"
    );
}

/// The EMA and the integral both count the window's length.
#[test]
fn a_2_s_window_counts_twice() {
    let mut l = LevelLoop::default();
    l.update(10.0, 2.0, 0.0);
    assert!((l.integral_ppm() - 0.0002 * 10.0 * 2.0).abs() < 1e-15);
    assert!((l.ema_ms() - 10.0 * 2.0 / 12.0).abs() < 1e-12);
}

#[test]
fn the_integral_is_clamped_and_frozen_while_the_sum_saturates() {
    let mut l = LevelLoop::default();
    for _ in 0..10_000 {
        l.update(100.0, 1.0, 0.0);
    }
    assert_eq!(l.integral_ppm(), LEVEL_INTEGRAL_MAX_PPM);
    let mut l = LevelLoop::default();
    for _ in 0..10_000 {
        l.update(-100.0, 1.0, 0.0);
    }
    assert_eq!(l.integral_ppm(), -LEVEL_INTEGRAL_MAX_PPM);
    let mut frozen = LevelLoop::default();
    frozen.update(10.0, 1.0, 290.0); // 290 + P 1.8 + 0 < 300: I moves
    let moved = frozen.integral_ppm();
    assert!(moved > 0.0);
    frozen.update(10.0, 1.0, 299.0); // 299 + P + I ≥ 300: I frozen
    assert_eq!(frozen.integral_ppm(), moved);
    l.reset_error();
    assert_eq!(l.ema_ms(), 0.0);
}

/// A 10 s window halves the EMA's distance (alpha 0.5): an error of 10 ms is
/// a P of exactly 10, so 290 + 10 + 0 is exactly the bound and I stays.
#[test]
fn a_sum_of_exactly_300_ppm_freezes_the_integral() {
    let mut l = LevelLoop::default();
    assert_eq!(l.update(10.0, 10.0, 290.0), 10.0);
    assert_eq!(l.integral_ppm(), 0.0);
    let mut l = LevelLoop::default();
    assert_eq!(l.update(-10.0, 10.0, -290.0), -10.0);
    assert_eq!(l.integral_ppm(), 0.0);
}

/// I's own share counts toward the bound: rate 279.5 + P 20 + I 1 ≥ 300.
#[test]
fn the_integral_counts_toward_the_bound() {
    let mut l = LevelLoop::default();
    for _ in 0..500 {
        l.update(10.0, 1.0, 0.0);
    }
    let i = l.integral_ppm();
    assert!((i - 1.0).abs() < 1e-9, "{i}");
    l.update(10.0, 1.0, 279.5);
    assert_eq!(l.integral_ppm(), i);
}

/// Beyond the 1 ms calm zone the stop curve √(2 · 5 · (|e| − 1) · 1000);
/// inside it (its edge included) nothing.
#[test]
fn the_offset_slew_is_the_stop_curve_beyond_the_calm_zone() {
    let calm = CALM_ZONE_MS;
    assert_eq!(braking_ppm(0.0, calm), 0.0);
    assert_eq!(braking_ppm(0.5, calm), 0.0);
    assert_eq!(braking_ppm(1.0, calm), 0.0, "the edge");
    assert_eq!(braking_ppm(-1.0, calm), 0.0);
    assert_eq!(braking_ppm(1.5, calm), 70.71067811865476);
    assert_eq!(braking_ppm(3.0, calm), 141.4213562373095);
    assert_eq!(
        braking_ppm(-3.0, calm),
        -141.4213562373095,
        "more output's opposite"
    );
    assert_eq!(braking_ppm(3.0, 2.5), 70.71067811865476, "a wider zone");
}

/// The calm zone: 1 ms, or half the driver's callback period when longer.
#[test]
fn the_calm_zone_is_1_ms_or_half_a_callback_period() {
    assert_eq!(calm_zone_ms(128, 96_000.0), 1.0, "0.67 ms: the floor");
    assert_eq!(calm_zone_ms(192, 96_000.0), 1.0, "exactly 1 ms");
    assert_eq!(calm_zone_ms(512, 96_000.0), 2.6666666666666665);
    assert_eq!(calm_zone_ms(1024, 48_000.0), 10.666666666666666);
    assert_eq!(Servo::new(RATE, BASE_LATENCY_100NS).calm_ms(), 1.0);
    assert_eq!(
        Servo::new(RATE, BASE_LATENCY_100NS)
            .with_callback_frames(512)
            .calm_ms(),
        2.6666666666666665
    );
}

/// The time left: accelerate at 5 ppm/s to a peak (at most the room),
/// cruise, brake to the calm zone's edge (pins from a scratch model).
#[test]
fn the_slews_time_left_accelerates_cruises_and_brakes() {
    let eta = |e, share, room| slew_eta_s(e, share, room, CALM_ZONE_MS).unwrap();
    assert_eq!(
        slew_eta_s(1.0, 0.0, 300.0, CALM_ZONE_MS),
        None,
        "the calm zone's edge"
    );
    assert_eq!(slew_eta_s(-1.0, 50.0, 300.0, CALM_ZONE_MS), None);
    assert_eq!(slew_eta_s(3.0, 0.0, 300.0, 3.0), None, "a wider zone");
    assert_eq!(slew_eta_s(4.0, 0.0, 300.0, 2.0), Some(40.0));
    assert_eq!(
        eta(3.0, 0.0, 300.0),
        40.0,
        "from rest: 2000 ppm·s, peak 100"
    );
    assert_eq!(
        eta(2.0, 100.0, 300.0),
        20.0,
        "its stop covers it exactly: brake"
    );
    assert!((eta(3.0, 100.0, 300.0) - 28.98979485566356).abs() < 1e-9);
    assert!(
        (eta(-3.0, -100.0, 300.0) - 28.98979485566356).abs() < 1e-9,
        "the same toward a negative error"
    );
    assert!(
        (eta(3.0, -100.0, 300.0) - 68.98979485566356).abs() < 1e-9,
        "the wrong way: stop first"
    );
    assert!(
        (eta(34.3333, 0.0, 300.0) - 171.111).abs() < 1e-9,
        "a slot: 60 s up, 51.1 s at 300 ppm, 60 s down"
    );
    assert!(
        (eta(34.3333, 0.0, 250.0) - 183.3332).abs() < 1e-9,
        "less room"
    );
    assert!((eta(-34.3333, 120.0, 300.0) - 199.911).abs() < 1e-9);
    assert!(
        (eta(11.0, 400.0, 300.0) - 63.333333333333336).abs() < 1e-9,
        "a share over the room counts as the room"
    );
}

#[test]
fn the_slew_is_5_ppm_per_second_and_never_backwards_in_time() {
    assert_eq!(slew(0.0, 100.0, 1.0), 5.0);
    assert_eq!(slew(0.0, 100.0, 0.5), 2.5);
    assert_eq!(slew(10.0, 7.0, 1.0), 7.0);
    assert_eq!(slew(10.0, -100.0, 2.0), 0.0);
    assert_eq!(
        slew(10.0, 100.0, -1.0),
        10.0,
        "a backward wall moves nothing"
    );
}

const RATE: f64 = 96_000.0;
const T0: i64 = 17_900_000_000_000_000;

/// A block handled `late_100ns` after its boundary `k` on a wall from `t0`,
/// with `buffered` frames waiting and the card at `consumed`.
fn obs_at(t0: i64, k: i64, late_100ns: i64, buffered: u64, consumed: u64) -> Observation {
    Observation {
        handled_100ns: t0 + k * SLOT_100NS + late_100ns,
        stamp_100ns: t0 + k * SLOT_100NS,
        buffered_frames: buffered,
        pending_skip_frames: 0,
        consumed_frames: consumed,
    }
}

fn obs(k: i64, late_100ns: i64, buffered: u64, consumed: u64) -> Observation {
    obs_at(T0, k, late_100ns, buffered, consumed)
}

/// An observation handled at its boundary `at` (100 ns).
fn at(at: i64, buffered: u64, consumed: u64) -> Observation {
    Observation {
        handled_100ns: at,
        stamp_100ns: at,
        buffered_frames: buffered,
        pending_skip_frames: 0,
        consumed_frames: consumed,
    }
}

#[test]
fn frames_convert_to_100ns() {
    assert_eq!(frames_to_100ns(96_000, RATE), 10_000_000);
    assert_eq!(frames_to_100ns(128, RATE), 13_333);
    assert_eq!(frames_to_100ns(0, RATE), 0);
    assert_eq!(frames_to_100ns(5_440, RATE), 566_667, "rounded");
    assert_eq!(frames_to_100ns(-4_800, RATE), -500_000, "owed");
}

#[test]
fn frames_convert_from_100ns() {
    assert_eq!(frames_from_100ns(10_000_000, RATE), 96_000);
    assert_eq!(frames_from_100ns(13_333, RATE), 128, "rounded");
    assert_eq!(frames_from_100ns(-666_666, RATE), -6_400, "a skip");
    assert_eq!(frames_from_100ns(0, RATE), 0);
    assert_eq!(frames_from_100ns(10_000_000, 44_100.0), 44_100);
}

#[test]
fn the_target_is_the_one_given() {
    assert_eq!(Servo::new(RATE, 777_777).target_100ns(), 777_777);
}

/// The first block primes the ring to the target — inserted like a
/// re-centre, but none: no fault is counted.
#[test]
fn the_first_block_primes_the_ring_and_is_no_re_centre() {
    let mut s = Servo::new(RATE, BASE_LATENCY_100NS);
    let a = s.observe(obs(0, 50_000, 0, 0));
    assert_eq!(
        a,
        ServoAction {
            correction_ppm: 0.0,
            recentre_100ns: BASE_LATENCY_100NS - 50_000,
            recentre: Some(Recentre::Prime),
        }
    );
    assert_eq!(s.status().hard_recentres, 0);
}

#[test]
fn a_hard_re_centre_is_a_deficit_over_50_ms_or_an_excess_over_4_slots() {
    assert_eq!(hard_recentre(0), None);
    assert_eq!(hard_recentre(500_000), None);
    assert_eq!(hard_recentre(500_001), Some(Recentre::Deficit));
    assert_eq!(hard_recentre(-1_333_333), None);
    assert_eq!(hard_recentre(-1_333_334), Some(Recentre::Excess));
    assert_eq!(
        [Recentre::Prime, Recentre::Deficit, Recentre::Excess].map(Recentre::as_str),
        ["prime", "deficit", "excess"]
    );
}

/// After the priming: a block exactly 50 ms short is left to the slew, one
/// 100 ns more is a hard insert; exactly 4 slots over is left, one more is a
/// hard skip. Each counted as a fault.
#[test]
fn a_block_past_the_last_resorts_edge_re_centres_hard() {
    let mut s = Servo::new(RATE, BASE_LATENCY_100NS);
    s.observe(obs(0, 0, 0, 0)); // the priming
    // buffered 0: the latency is the hand-off lateness alone.
    let a = s.observe(obs(1, 166_666, 0, 3_200));
    assert_eq!((a.recentre_100ns, a.recentre), (0, None));
    let b = s.observe(obs(2, 166_665, 0, 6_400));
    assert_eq!(
        (b.recentre_100ns, b.recentre),
        (500_001, Some(Recentre::Deficit))
    );
    assert_eq!(s.status().hard_recentres, 1);
    let c = s.observe(obs(3, 1_999_999, 0, 9_600));
    assert_eq!((c.recentre_100ns, c.recentre), (0, None));
    let d = s.observe(obs(4, 2_000_000, 0, 12_800));
    assert_eq!(
        (d.recentre_100ns, d.recentre),
        (-1_333_334, Some(Recentre::Excess))
    );
    assert_eq!(s.status().hard_recentres, 2);
}

/// How [`steady`] feeds the servo: blocks on a wall from `t0`, each handled
/// `late_100ns` after its boundary with `buffered` frames waiting, the card
/// at `ppm` from `offset` frames.
#[derive(Clone, Copy)]
struct Feed {
    t0: i64,
    late_100ns: i64,
    buffered: u64,
    ppm: f64,
    offset: u64,
}

const FEED: Feed = Feed {
    t0: T0,
    late_100ns: 0,
    buffered: 6_400,
    ppm: 0.0,
    offset: 0,
};

/// Blocks `from..from + n` fed as `f` says; the last action.
fn steady(s: &mut Servo, from: i64, n: i64, f: Feed) -> ServoAction {
    let mut last = ServoAction {
        correction_ppm: 0.0,
        recentre_100ns: 0,
        recentre: None,
    };
    for k in from..from + n {
        let elapsed_s = (k * SLOT_100NS) as f64 / 1e7;
        let consumed = f.offset + (elapsed_s * RATE * (1.0 + f.ppm * 1e-6)) as u64;
        last = s.observe(obs_at(f.t0, k, f.late_100ns, f.buffered, consumed));
    }
    last
}

/// The latency 20 ms low for a whole window: nothing is spliced (the
/// owner's ruling: no skip, no insert); the window closes at block 32 and the
/// slew starts — 5 ppm per second of the window, toward the stop curve's
/// 435.9 ppm (clamped at 300). The status shows the offset and the time left.
#[test]
fn a_window_mean_20_ms_low_is_slewed_never_spliced() {
    let mut s = Servo::new(RATE, BASE_LATENCY_100NS);
    s.observe(obs(0, 0, 6_400, 0));
    // 4_480 frames = 466_667: 199_999 under the target.
    let low = Feed {
        buffered: 4_480,
        ..FEED
    };
    assert_eq!(steady(&mut s, 1, 31, low), steady_hold());
    let a = steady(&mut s, 32, 1, low);
    assert_eq!((a.recentre_100ns, a.recentre), (0, None));
    assert!((a.correction_ppm - 5.1666615).abs() < 1e-9, "{a:?}");
    let st = s.status();
    assert_eq!((st.latency_ms, st.offset_ms), (46.6667, -19.9999), "{st:?}");
    assert!(
        (st.slew_eta_s.unwrap() - 122.30856583035187).abs() < 1e-9,
        "{st:?}"
    );
    // A minute more of it: the slew reaches the 300 ppm bound, never a splice.
    let a = steady(&mut s, 33, 30 * 60, low);
    assert_eq!((a.correction_ppm, a.recentre_100ns), (300.0, 0));
    assert_eq!(s.status().hard_recentres, 0);
}

/// What [`steady`] returns while no window closed yet.
fn steady_hold() -> ServoAction {
    ServoAction {
        correction_ppm: 0.0,
        recentre_100ns: 0,
        recentre: None,
    }
}

/// A skip longer than one block runs over several: frames still to skip are
/// not counted as buffered, so the servo does not ask for them again (150 ms
/// over: past the last resort's 4 slots).
#[test]
fn a_pending_skip_is_not_asked_for_again() {
    let mut s = Servo::new(RATE, BASE_LATENCY_100NS);
    s.observe(obs(0, 0, 6_400, 0));
    let mut o = obs(1, 0, 6_400 + 14_400, 3_200);
    let ask = s.observe(o);
    assert_eq!(
        (ask.recentre_100ns, ask.recentre),
        (BASE_LATENCY_100NS - 2_166_667, Some(Recentre::Excess)),
        "skip 150 ms"
    );
    o = Observation {
        pending_skip_frames: 14_400,
        ..obs(2, 0, 6_400 + 14_400, 6_400)
    };
    assert_eq!(s.observe(o).recentre_100ns, 0, "already being skipped");
    assert_eq!(s.status().hard_recentres, 1);
}

/// A worker stall: blocks 31–35 are all handled at block 31's boundary +
/// 250 ms, the ring down to the splice's hold (480 frames). The first is
/// 188.3 ms over (past the last resort's 4 slots) and asks for a hard skip;
/// while it runs the frames still to skip outnumber the buffered ones (to
/// play: negative) and nothing more is asked.
#[test]
fn a_skip_after_a_stall_is_not_asked_again_while_it_outruns_the_ring() {
    let mut s = Servo::new(RATE, BASE_LATENCY_100NS);
    s.observe(obs(0, 0, 6_400, 0));
    steady(&mut s, 1, 30, FEED);
    let when = T0 + 31 * SLOT_100NS + 2_500_000;
    let mut asked = Vec::new();
    // The skip: 18_080 frames, each block's 3_200 eaten in turn.
    for (k, buffered, pending) in [
        (31, 480, 0),
        (32, 480, 14_880),
        (33, 480, 11_680),
        (34, 480, 8_480),
        (35, 480, 5_280),
    ] {
        let a = s.observe(Observation {
            handled_100ns: when,
            stamp_100ns: T0 + k * SLOT_100NS,
            buffered_frames: buffered,
            pending_skip_frames: pending,
            consumed_frames: 113_599,
        });
        asked.push(a.recentre_100ns);
    }
    assert_eq!(asked, [-1_883_334, 0, 0, 0, 0]);
    assert_eq!(s.status().hard_recentres, 1);
}

/// The wall steps back 2.5 s inside a window: the window starts over at the
/// new reading (closing 1 s on), not 1 s past the old start.
#[test]
fn a_wall_stepped_back_inside_a_window_starts_it_over() {
    let mut s = Servo::new(RATE, BASE_LATENCY_100NS);
    s.observe(at(T0, 6_400, 0));
    s.observe(at(T0 + 1_000_000, 6_400, 9_600));
    s.observe(at(T0 + 5_000_000, 6_400, 48_000));
    s.observe(at(T0 - 20_000_000, 6_400, 52_800));
    assert_eq!(s.status().latency_ms, 0.0, "open");
    s.observe(at(T0 - 10_000_000, 6_400, 148_800));
    assert_eq!(
        s.status().latency_ms,
        66.6667,
        "closed 1 s after the new start"
    );
}

/// Two blocks handled at one instant (a burst) both count in the window.
#[test]
fn blocks_handled_at_one_instant_share_a_window() {
    let mut s = Servo::new(RATE, BASE_LATENCY_100NS);
    s.observe(at(T0, 6_400, 0));
    // 6_400 frames = 666_667; 6_496 = 676_667.
    s.observe(at(T0 + 1_000_000, 6_400, 9_600));
    s.observe(at(T0 + 1_000_000, 6_496, 9_600));
    s.observe(at(T0 + 11_000_000, 6_400, 105_600));
    assert_eq!(s.status().latency_ms, 67.0, "the mean of three");
}

/// The wall steps back 11 s right at a window's close: the next window
/// closes 10 s before the last one. No time passed for the level loop — the
/// correction holds, finite (an EMA over −10 s would divide by zero).
#[test]
fn a_wall_stepped_back_past_the_last_window_moves_nothing() {
    let mut s = Servo::new(RATE, BASE_LATENCY_100NS);
    // 5_920 frames: the level 5 ms low, so the level loop has work to do.
    s.observe(at(T0, 5_920, 0));
    s.observe(at(T0 + 1_000_000, 5_920, 9_600));
    s.observe(at(T0 + 11_000_000, 5_920, 105_600));
    let before = s.status().correction_ppm;
    assert!(before > 0.0, "{before}");
    s.observe(at(T0 - 99_000_000, 5_920, 115_200));
    s.observe(at(T0 - 89_000_000, 5_920, 211_200));
    let after = s.status();
    assert_eq!(after.latency_ms, 61.6667, "still 5 ms low");
    assert_eq!(after.correction_ppm, before, "{after:?}");
    // The next window, 1.1 s on: the loop goes on, finite.
    s.observe(at(T0 - 88_000_000, 5_920, 220_800));
    s.observe(at(T0 - 78_000_000, 5_920, 316_800));
    let next = s.status().correction_ppm;
    assert!(next.is_finite() && next > before, "{next} after {before}");
}

/// A window closes once it spans exactly 1 s (its latency is then reported).
#[test]
fn a_window_closes_at_exactly_one_second() {
    let mut s = Servo::new(RATE, BASE_LATENCY_100NS);
    s.observe(at(T0, 6_400, 0));
    s.observe(at(T0 + 1_000_000, 6_400, 9_600));
    assert_eq!(s.status().latency_ms, 0.0, "open");
    s.observe(at(T0 + 10_999_999, 6_400, 105_600));
    assert_eq!(s.status().latency_ms, 0.0, "a hair short of 1 s");
    s.observe(at(T0 + 11_000_000, 6_400, 105_600));
    assert_eq!(s.status().latency_ms, 66.6667, "closed at exactly 1 s");
}

/// A 65 536 Hz card makes every window value exact: the card 0.125 s ahead
/// of the wall over 1.25 s is exactly 100 000 ppm — still a measurement (its
/// latency is reported); one frame more is starved (flushed, not reported).
#[test]
fn a_window_of_exactly_100_000_ppm_is_a_measurement() {
    for (extra, kept) in [(0, true), (1, false)] {
        // 4_369 frames at 65 536 Hz = 666_656 (100 ns): 10 under the target.
        let mut s = Servo::new(65_536.0, BASE_LATENCY_100NS);
        s.observe(at(T0, 4_369, 0));
        s.observe(at(T0 + 10_000_000, 4_369, 65_536));
        s.observe(at(T0 + 22_500_000, 4_369, 65_536 + 90_112 + extra));
        assert_eq!(s.status().latency_ms != 0.0, kept, "{extra} frame(s) more");
    }
}

#[test]
fn a_fast_card_gets_a_positive_correction_once_locked() {
    // The card had already played 5 s when the first block came.
    let mut s = Servo::new(RATE, BASE_LATENCY_100NS);
    s.observe(obs(0, 0, 6_400, 480_000));
    let card = Feed {
        ppm: 40.0,
        offset: 480_000,
        ..FEED
    };
    steady(&mut s, 1, 30 * 90, card); // 90 s
    let st = s.status();
    assert!(st.locked);
    assert!((st.rate_ppm - 40.0).abs() < 0.5, "{st:?}");
    assert!(
        st.correction_ppm > 35.0 && st.correction_ppm < 45.0,
        "more output for a fast card, slewed to it: {st:?}"
    );
    assert_eq!((st.hard_recentres, st.rebases), (0, 0), "{st:?}");
    assert_eq!(st.latency_ms, 66.6667);
}

/// A 200 ppm card: once locked, the correction climbs to it at most 5 ppm per
/// second of the wall.
#[test]
fn the_correction_moves_at_most_5_ppm_per_second() {
    let mut s = Servo::new(RATE, BASE_LATENCY_100NS);
    s.observe(obs(0, 0, 6_400, 0));
    let mut last_change: Option<(f64, i64)> = None;
    let mut worst = 0.0f64;
    for k in 1..30 * 130 {
        let elapsed_s = (k * SLOT_100NS) as f64 / 1e7;
        let consumed = (elapsed_s * RATE * (1.0 + 200e-6)) as u64;
        let a = s.observe(obs(k, 0, 6_400, consumed));
        if let Some((ppm, since)) = last_change
            && a.correction_ppm != ppm
        {
            let dt_s = ((k - since) * SLOT_100NS) as f64 / 1e7;
            worst = worst.max((a.correction_ppm - ppm).abs() - MAX_SLEW_PPM_PER_S * dt_s);
        }
        if last_change.is_none_or(|(ppm, _)| ppm != a.correction_ppm) {
            last_change = Some((a.correction_ppm, k));
        }
    }
    assert!(worst <= 1e-9, "{worst}");
    assert!(s.status().correction_ppm > 150.0, "{:?}", s.status());
}

/// The same card, seen from the lock: the correction never runs ahead of
/// 5 ppm per second since then (one window's move of slack).
#[test]
fn the_correction_climbs_from_the_lock_at_5_ppm_per_second() {
    let mut s = Servo::new(RATE, BASE_LATENCY_100NS);
    s.observe(obs(0, 0, 6_400, 0));
    let mut locked_at = None;
    for k in 1..30 * 100 {
        let elapsed_s = (k * SLOT_100NS) as f64 / 1e7;
        let consumed = (elapsed_s * RATE * (1.0 + 200e-6)) as u64;
        let a = s.observe(obs(k, 0, 6_400, consumed));
        if locked_at.is_none() && s.status().locked {
            locked_at = Some(k);
        }
        let since_s = locked_at.map_or(0.0, |l| ((k - l) * SLOT_100NS) as f64 / 1e7);
        assert!(
            a.correction_ppm <= MAX_SLEW_PPM_PER_S * (since_s + 1.1),
            "block {k}: {a:?}"
        );
    }
    assert!(locked_at.is_some());
}

/// A 0 ppm card with the level held 5 ms low (5_920 frames = 616_667 of
/// 666_666): 4 ms beyond the calm zone, the offset slew asks for 200 ppm
/// (more output), so the correction climbs at the slew limit — 5 ppm per
/// second of the 29 windows closed in 30 s, before any lock. Pinned with two
/// independent scratch derivations.
#[test]
fn a_level_held_5_ms_low_is_slewed_at_the_limit() {
    let mut s = Servo::new(RATE, BASE_LATENCY_100NS);
    s.observe(obs(0, 0, 6_400, 0));
    let low = Feed {
        buffered: 5_920,
        ..FEED
    };
    let a = steady(&mut s, 1, 30 * 30, low);
    assert!(!s.status().locked);
    assert_eq!(s.status().latency_ms, 61.6667);
    assert!(
        (a.correction_ppm - 149.16651749999994).abs() < 1e-9,
        "{a:?}"
    );
    assert_eq!(s.status().hard_recentres, 0);
    // 90 s more: the correction reaches its unclamped target — the card's
    // rate (locked, ~0) + the stop curve's 200 ppm + P (~10) + I.
    let a = steady(&mut s, 30 * 30 + 1, 30 * 90, low);
    assert!((a.correction_ppm - 210.1185091994812).abs() < 1e-9, "{a:?}");
}

/// A 40 ppm card, locked, then the level 20 ms low for two windows: the
/// slew's time left counts only the share above the card's rate and the room
/// the rate leaves in the ±300 budget (pins from the scratch model).
#[test]
fn the_time_left_counts_from_the_cards_rate() {
    let mut s = Servo::new(RATE, BASE_LATENCY_100NS);
    s.observe(obs(0, 0, 6_400, 480_000));
    let card = Feed {
        ppm: 40.0,
        offset: 480_000,
        ..FEED
    };
    steady(&mut s, 1, 30 * 90, card);
    let low = Feed {
        buffered: 4_480,
        ..card
    };
    let a = steady(&mut s, 30 * 90 + 1, 64, low);
    let st = s.status();
    assert!(st.locked, "{st:?}");
    assert!((a.correction_ppm - 50.66754506554876).abs() < 1e-9, "{a:?}");
    assert_eq!(st.offset_ms, -19.9999);
    assert!(
        (st.slew_eta_s.unwrap() - 122.98701246771205).abs() < 1e-9,
        "{st:?}"
    );
}

#[test]
fn a_starved_window_flushes_the_rate_and_holds_the_correction() {
    let mut s = Servo::new(RATE, BASE_LATENCY_100NS);
    s.observe(obs(0, 0, 6_400, 0));
    let card = Feed { ppm: 20.0, ..FEED };
    steady(&mut s, 1, 30 * 70, card);
    let before = s.status();
    assert!(before.locked);
    // The card stops consuming for two windows (a vanished driver): each
    // window measures about −1e6 ppm.
    let frozen = (70.0 * RATE * (1.0 + 20e-6)) as u64;
    let mut held = steady_hold();
    for k in 30 * 70 + 1..30 * 70 + 1 + 62 {
        held = s.observe(obs(k, 0, 6_400, frozen));
    }
    let after = s.status();
    assert!(!after.locked, "the regression starts over");
    assert_eq!(after.rate_ppm, 0.0);
    assert_eq!(
        held.correction_ppm, before.correction_ppm,
        "held, not chased: {held:?} {before:?}"
    );
}

/// The servo measures its time from its first block: a wall far from zero
/// (past half of i64) is measured like any other.
#[test]
fn a_wall_far_from_zero_is_measured_from_the_first_block() {
    let t0 = 4_700_000_000_000_000_000;
    let mut s = Servo::new(RATE, BASE_LATENCY_100NS);
    s.observe(obs_at(t0, 0, 0, 6_400, 0));
    steady(
        &mut s,
        1,
        30 * 70,
        Feed {
            t0,
            ppm: -30.0,
            ..FEED
        },
    );
    let st = s.status();
    assert!(st.locked);
    assert!((st.rate_ppm + 30.0).abs() < 0.5, "{st:?}");
}

/// The card's position jumps 20 ms (a burst of lost callbacks) while the
/// latency holds: the regression re-bases once and keeps the rate.
#[test]
fn a_jump_of_the_cards_position_rebases_once() {
    let mut s = Servo::new(RATE, BASE_LATENCY_100NS);
    s.observe(obs(0, 0, 6_400, 0));
    let card = Feed { ppm: 10.0, ..FEED };
    steady(&mut s, 1, 30 * 80, card);
    assert!(s.status().locked);
    assert_eq!(s.status().rebases, 0);
    let jumped = Feed {
        offset: 1_920,
        ..card
    };
    steady(&mut s, 30 * 80 + 1, 30 * 20, jumped);
    let st = s.status();
    assert_eq!((st.rebases, st.hard_recentres), (1, 0), "{st:?}");
    assert!((st.rate_ppm - 10.0).abs() < 0.5, "{st:?}");
}
