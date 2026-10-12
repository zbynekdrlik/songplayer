//! #233 (the SoundGrid loop check): the search finds which input carries
//! each sent channel, the round trip to the exact frame and the gain (its
//! polarity included), at the range's edges and between coarse steps;
//! nothing locks on a return that is not the sent one; its helpers are
//! exact; its work is bounded.

use super::*;

/// Noise in [−0.5, 0.5), exact in f32 (a 24-bit LCG word / 2^24 − 0.5).
fn noise(seed: u64, n: usize) -> Vec<f32> {
    let mut x = seed;
    (0..n)
        .map(|_| {
            x = x
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (x >> 40) as f32 / 16_777_216.0 - 0.5
        })
        .collect()
}

/// The window an input returns `sent` in, `delay` frames later, × `gain`.
fn back(sent: &[f32], max_lag: usize, delay: usize, gain: f32) -> Vec<f32> {
    sent[max_lag - delay..][..WINDOW]
        .iter()
        .map(|&x| x * gain)
        .collect()
}

/// L and R, each `WINDOW + max_lag` frames of their own noise.
fn sent(max_lag: usize) -> [Vec<f32>; 2] {
    [noise(1, WINDOW + max_lag), noise(2, WINDOW + max_lag)]
}

fn run(sent: &[Vec<f32>; 2], recv: &[Vec<f32>], max_lag: usize) -> (Option<Found>, u64) {
    let recv: Vec<&[f32]> = recv.iter().map(Vec::as_slice).collect();
    let mut work = 0;
    let found = search(
        [sent[0].as_slice(), sent[1].as_slice()],
        &recv,
        max_lag,
        &mut work,
    );
    (found, work)
}

fn tap(input: usize, gain: f64) -> Option<Tap> {
    Some(Tap { input, gain })
}

#[test]
fn it_finds_each_channel_its_input_its_round_trip_and_its_gain() {
    let lag = 4_000;
    let s = sent(lag);
    // Input 0 carries R inverted at half its level, input 1 nothing, input
    // 2 the sent L: 1 234 frames later (between two coarse steps).
    let recv = vec![
        back(&s[1], lag, 1_234, -0.5),
        vec![0.0; WINDOW],
        back(&s[0], lag, 1_234, 1.0),
    ];
    let (found, work) = run(&s, &recv, lag);
    assert_eq!(
        found,
        Some(Found {
            delay: 1_234,
            taps: [tap(2, 1.0), tap(0, -0.5)],
        })
    );
    // The CPU bound: every coarse lag of every input and channel, at most
    // four refined neighbourhoods, the assignment.
    let coarse = 3 * 2 * (lag / DECIMATE + 1) * (WINDOW / DECIMATE);
    let assign = 2 * 3 * 2 * WINDOW;
    let refine_max = CANDIDATES * (2 * REFINE_FRAMES + 1) * WINDOW;
    let work = work as usize;
    assert!(
        work >= coarse + (2 * REFINE_FRAMES + 1) * WINDOW + assign,
        "{work}"
    );
    assert!(work <= coarse + refine_max + assign, "{work}");
}

/// The bound the module doc states, at 96 kHz with 8 inputs.
#[test]
fn an_attempt_at_96_khz_with_eight_inputs_is_bounded() {
    let bound = |max_lag: usize, inputs: usize| {
        2 * inputs * (max_lag / DECIMATE + 1) * (WINDOW / DECIMATE)
            + CANDIDATES * (2 * REFINE_FRAMES + 1) * WINDOW
            + 2 * 2 * inputs * WINDOW
    };
    assert_eq!(bound(48_000, 8), 24_584_192 + 2_129_920 + 262_144);
    assert!(bound(48_000, 8) < 27_000_000);
}

/// One lag, one candidate: the exact work, every loop counted once.
#[test]
fn the_work_counts_every_correlation_once() {
    let s = sent(0);
    let recv = vec![
        back(&s[0], 0, 0, 1.0),
        back(&s[1], 0, 0, 1.0),
        vec![0.0; WINDOW],
    ];
    let (found, work) = run(&s, &recv, 0);
    assert_eq!(
        found,
        Some(Found {
            delay: 0,
            taps: [tap(0, 1.0), tap(1, 1.0)],
        })
    );
    // Coarse 3 inputs × 2 channels × 1 lag × 512, refine 1 lag × 8192,
    // assign 2 channels × 3 inputs × 2 × 8192.
    assert_eq!(work, 3 * 2 * 512 + 8_192 + 2 * 3 * 2 * 8_192);
}

#[test]
fn the_range_s_edges_and_a_round_trip_under_a_coarse_step() {
    let lag = 4_000;
    let s = sent(lag);
    for delay in [0, 5, 31, 33, 3_999, 4_000] {
        let recv = vec![back(&s[0], lag, delay, 1.0), back(&s[1], lag, delay, 1.0)];
        let (found, _) = run(&s, &recv, lag);
        assert_eq!(
            found.map(|f| (f.delay, f.taps)),
            Some((delay, [tap(0, 1.0), tap(1, 1.0)])),
            "{delay}"
        );
    }
}

#[test]
fn only_one_channel_returning_is_a_lock_of_that_channel() {
    let lag = 4_000;
    let s = sent(lag);
    let recv = vec![back(&s[1], lag, 700, 0.25)];
    let (found, _) = run(&s, &recv, lag);
    assert_eq!(
        found,
        Some(Found {
            delay: 700,
            taps: [None, tap(0, 0.25)],
        })
    );
}

#[test]
fn nothing_locks_on_a_return_that_is_not_the_sent_one() {
    let lag = 4_000;
    let s = sent(lag);
    // Silence, unrelated noise, and the sent signal from beyond the range.
    for recv in [
        vec![vec![0.0; WINDOW]],
        vec![noise(3, WINDOW), noise(4, WINDOW)],
    ] {
        assert_eq!(run(&s, &recv, lag).0, None);
    }
    let wide = sent(4_016);
    let beyond = vec![back(&wide[0], 4_016, 4_016, 1.0)];
    let narrow = [wide[0][16..].to_vec(), wide[1][16..].to_vec()];
    assert_eq!(run(&narrow, &beyond, lag).0, None, "beyond the range");
    // A silent program: no lag has energy to search by.
    let quiet = [vec![0.0; WINDOW + lag], vec![0.0; WINDOW + lag]];
    let (found, work) = run(&quiet, &[vec![0.0; WINDOW]], lag);
    assert_eq!((found, work), (None, 0));
}

/// A lock needs the window to pass the judge's test: one sample off by 0.8
/// is a residual of ~−30 dB of what was expected (no lock); off by 0.01,
/// ~−68 dB (a lock).
#[test]
fn a_window_that_would_read_as_a_mismatch_never_locks() {
    let lag = 64;
    let s = sent(lag);
    let mut recv = vec![back(&s[0], lag, 20, 1.0)];
    recv[0][4_000] += 0.8;
    assert_eq!(run(&s, &recv, lag).0, None);
    let mut recv = vec![back(&s[0], lag, 20, 1.0)];
    recv[0][4_000] += 0.01;
    assert_eq!(run(&s, &recv, lag).0.map(|f| f.delay), Some(20));
}

#[test]
fn a_window_of_any_other_shape_is_none() {
    let lag = 64;
    let s = sent(lag);
    let good = vec![back(&s[0], lag, 20, 1.0)];
    assert_eq!(run(&s, &good, lag).0.map(|f| f.delay), Some(20));
    // No input, a range that is no whole coarse step, a short window, a
    // short sent side.
    assert_eq!(run(&s, &[], lag).0, None);
    let odd = [s[0][1..].to_vec(), s[1][1..].to_vec()];
    assert_eq!(run(&odd, &good, lag - 1).0, None);
    let short = vec![good[0][1..].to_vec()];
    assert_eq!(run(&s, &short, lag).0, None);
    let cut = [s[0][1..].to_vec(), s[1].clone()];
    assert_eq!(run(&cut, &good, lag).0, None);
}

#[test]
fn the_helpers_are_exact() {
    let x = [1.0f32, -2.0, 0.5, 3.0];
    assert_eq!(prefix_squares(&x), vec![0.0, 1.0, 5.0, 5.25, 14.25]);
    let p = prefix_squares(&x);
    assert_eq!(energy(&p, 1, 2), 4.25);
    assert_eq!(energy(&p, 0, 4), 14.25);
    assert_eq!(dot(&x, &[2.0, 1.0, 4.0, -1.0]), -1.0);
    assert_eq!(dot64(&[1.5, -2.0], &[2.0, 3.0]), -3.0);
    let ramp: Vec<f32> = (0..40).map(|i| i as f32).collect();
    // Two whole steps of 16 (0..16, 16..32); the 8 left over are dropped.
    assert_eq!(box_sums(&ramp), vec![120.0, 376.0]);
    assert_eq!(correlation(2.0, 1.0, 4.0), 1.0);
    assert_eq!(correlation(-3.0, 4.0, 9.0), 0.5);
    assert_eq!(correlation(0.0, 0.0, 0.0), 0.0, "a silent side");
}

fn cand(score: f64, lag: usize) -> Candidate {
    Candidate {
        score,
        lag,
        input: lag % 3,
        channel: 0,
    }
}

#[test]
fn the_best_lags_are_kept_one_per_neighbourhood() {
    let mut top = Vec::new();
    offer(&mut top, cand(0.5, 100));
    // Within 32 frames: replaces only when better; at 33, a lag of its own.
    offer(&mut top, cand(0.4, 132));
    offer(&mut top, cand(0.5, 68));
    assert_eq!(top, vec![cand(0.5, 100)], "worse, or not better: dropped");
    offer(&mut top, cand(0.6, 132));
    assert_eq!(top, vec![cand(0.6, 132)]);
    offer(&mut top, cand(0.1, 165));
    offer(&mut top, cand(0.3, 1_000));
    offer(&mut top, cand(0.2, 2_000));
    assert_eq!(
        top,
        vec![
            cand(0.6, 132),
            cand(0.1, 165),
            cand(0.3, 1_000),
            cand(0.2, 2_000)
        ]
    );
    // Full: a better far one takes the weakest's place, a worse one none.
    offer(&mut top, cand(0.05, 3_000));
    offer(&mut top, cand(0.1, 3_000));
    assert_eq!(top[1], cand(0.1, 165), "not better than the weakest");
    offer(&mut top, cand(0.7, 3_000));
    assert_eq!(
        top,
        vec![
            cand(0.6, 132),
            cand(0.7, 3_000),
            cand(0.3, 1_000),
            cand(0.2, 2_000)
        ]
    );
    assert_eq!(top.len(), CANDIDATES);
}

/// A program with no time structure (DC, exactly representable: every lag
/// correlates exactly 1) locks at the FIRST of its equal lags, on the first
/// of two equal inputs — a later equal one never replaces it (the known
/// limit of a periodic program, `asio_loop.rs`).
#[test]
fn equal_lags_and_equal_inputs_keep_the_first() {
    let lag = 64;
    let dc = [vec![0.25f32; WINDOW + lag], vec![0.25f32; WINDOW + lag]];
    let recv = vec![vec![0.25f32; WINDOW], vec![0.25f32; WINDOW]];
    assert_eq!(
        run(&dc, &recv, lag).0,
        Some(Found {
            delay: 0,
            taps: [tap(0, 1.0), tap(0, 1.0)],
        })
    );
}
