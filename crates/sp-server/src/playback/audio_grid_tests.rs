//! `AudioGridBuffer` tests (#148) — the planar-FIFO fractional-reader that
//! delivers exactly `samples_per_boundary` samples per grid boundary and
//! slow-resamples the file-clock residual via a linear-interpolation pointer.
//! `super::*` resolves to the `audio_grid` module under test.

use super::*;

/// A one-channel planar chunk of a constant value.
fn const_chunk(value: f32, n: usize) -> Vec<Vec<f32>> {
    vec![vec![value; n]]
}

#[test]
fn every_boundary_yields_exactly_the_requested_samples_and_level_stays_bounded() {
    // 23.976-fps source (2002 samples/frame) paced onto a 30-fps grid
    // (1600 samples/boundary): 300 boundaries (10 s), ~240 source frames.
    let mut buf = AudioGridBuffer::new(48_000, 3200);
    // Prime with 4 frames so the reader never starves under the ~0.8 push/take cadence.
    let mut pushed = 0usize;
    for _ in 0..4 {
        buf.push(&const_chunk(0.5, 2002));
        pushed += 1;
    }
    for k in 1..=300i64 {
        // Even 240-chunks-over-300-boundaries cadence.
        let want = (k * 240) / 300;
        while (pushed as i64) < want + 4 {
            buf.push(&const_chunk(0.5, 2002));
            pushed += 1;
        }
        let out = buf.take_boundary_chunk(1600);
        assert_eq!(out.len(), 1, "one channel");
        assert_eq!(
            out[0].len(),
            1600,
            "each boundary yields exactly 1600 (k={k})"
        );
        assert!(
            buf.level_samples() < buf.cap_samples(),
            "level must stay below the 2 s cap (k={k}, level={})",
            buf.level_samples()
        );
    }
    assert_eq!(buf.underruns(), 0, "a well-fed buffer never underruns");
    assert_eq!(buf.overflows(), 0, "a well-fed buffer never overflows");
}

#[test]
fn starved_take_zero_fills_the_remainder_and_counts_one_underrun() {
    let mut buf = AudioGridBuffer::new(48_000, 3200);
    buf.push(&const_chunk(1.0, 800)); // only half a boundary's worth
    let out = buf.take_boundary_chunk(1600);
    assert_eq!(out[0].len(), 1600);
    for (i, s) in out[0].iter().enumerate().take(800) {
        assert!((s - 1.0).abs() < 1e-6, "real sample {i} = {s}");
    }
    for (i, s) in out[0].iter().enumerate().skip(800) {
        assert_eq!(*s, 0.0, "zero-fill sample {i}");
    }
    assert_eq!(buf.underruns(), 1, "one starved take = one underrun");
}

#[test]
fn flooding_past_the_two_second_cap_counts_overflows_and_bounds_the_level() {
    let mut buf = AudioGridBuffer::new(48_000, 3200);
    // Cap is 2 s = 96_000 samples; push 120_000 without draining.
    for _ in 0..60 {
        buf.push(&const_chunk(0.1, 2000));
    }
    assert!(
        buf.overflows() > 0,
        "flooding past the cap must count overflows"
    );
    assert!(
        buf.level_samples() <= buf.cap_samples(),
        "level must never exceed the cap: {} > {}",
        buf.level_samples(),
        buf.cap_samples()
    );
}

#[test]
fn fractional_reader_resamples_a_sine_up_by_three_hundred_ppm_without_discontinuity() {
    let mut buf = AudioGridBuffer::new(48_000, 3200);
    buf.set_applied_ppm(300.0);

    let f = 1000.0f64;
    let fs = 48_000.0f64;
    let sine = |gi: usize| ((2.0 * std::f64::consts::PI * f * gi as f64 / fs).sin()) as f32;

    let mut gi = 0usize;
    // Prime, then feed the sine continuously (1600 in / 1600 out per boundary).
    {
        let mut ch = Vec::with_capacity(8000);
        for _ in 0..8000 {
            ch.push(sine(gi));
            gi += 1;
        }
        buf.push(&[ch]);
    }
    let mut out_all: Vec<f32> = Vec::with_capacity(480_000);
    for _ in 0..300 {
        let mut ch = Vec::with_capacity(1600);
        for _ in 0..1600 {
            ch.push(sine(gi));
            gi += 1;
        }
        buf.push(&[ch]);
        let out = buf.take_boundary_chunk(1600);
        out_all.extend_from_slice(&out[0]);
    }
    assert_eq!(out_all.len(), 480_000, "10 s of output at 48 kHz");
    assert_eq!(buf.underruns(), 0, "the sine feed never starved");

    // Frequency via zero crossings over the 10 s of output: +300 ppm read step
    // shifts 1000 Hz to 1000.3 Hz.
    let mut crossings = 0usize;
    for w in out_all.windows(2) {
        if (w[0] < 0.0) != (w[1] < 0.0) {
            crossings += 1;
        }
    }
    let freq = crossings as f64 / 2.0 / 10.0;
    assert!(
        (freq - 1000.3).abs() < 0.05,
        "read at +300 ppm must be ~1000.3 Hz, got {freq}"
    );

    // No discontinuity: the max sample-to-sample delta stays below the sine's
    // own max slope (A·2π·f/fs ≈ 0.131), a jump at a take boundary would exceed it.
    let mut max_delta = 0.0f32;
    for w in out_all.windows(2) {
        max_delta = max_delta.max((w[1] - w[0]).abs());
    }
    assert!(
        max_delta < 0.15,
        "discontinuity detected: max delta {max_delta}"
    );
}

#[test]
fn underrun_keeps_the_remaining_fifo_samples_and_frac_pos() {
    // A fractional reader (step 1.5) starving on the interpolation partner must
    // KEEP the last real sample + frac_pos, not clear the FIFO (#148 rework).
    let mut buf = AudioGridBuffer::new(48_000, 3200);
    buf.set_applied_ppm(500_000.0); // step = 1.5
    buf.push(&[vec![10.0, 20.0]]); // two samples
    let out = buf.take_boundary_chunk(3);
    assert_eq!(buf.underruns(), 1, "starved mid-chunk = one underrun");
    assert!(
        (out[0][0] - 10.0).abs() < 1e-6,
        "first output is the real sample"
    );
    assert_eq!(out[0][1], 0.0, "the missing tail is zero-filled");
    assert_eq!(out[0][2], 0.0, "the missing tail is zero-filled");
    // The FIFO kept its last sample instead of clearing to empty.
    assert_eq!(
        buf.level_samples(),
        1,
        "the last (partner) sample must survive the underrun"
    );
}

#[test]
fn overflow_warning_fires_once_per_song_and_rearms_on_clear() {
    let mut buf = AudioGridBuffer::new(48_000, 3200);
    // No overflow yet → no warning.
    assert!(!buf.take_overflow_warning());
    // Flood past the 2 s cap to force overflows.
    for _ in 0..60 {
        buf.push(&const_chunk(0.1, 2000));
    }
    assert!(buf.overflows() > 0, "flooding overflows");
    assert!(buf.take_overflow_warning(), "first warning fires");
    assert!(
        !buf.take_overflow_warning(),
        "latched — no second warning this song"
    );
    // A new song (clear/anchor) re-arms the warning.
    buf.clear();
    for _ in 0..60 {
        buf.push(&const_chunk(0.1, 2000));
    }
    assert!(
        buf.take_overflow_warning(),
        "clear re-arms the per-song warning"
    );
}

#[test]
fn clear_empties_the_buffer_and_resets_the_reader() {
    let mut buf = AudioGridBuffer::new(48_000, 3200);
    buf.push(&const_chunk(1.0, 5000));
    buf.set_applied_ppm(200.0);
    assert!(buf.level_samples() > 0);
    buf.clear();
    assert_eq!(buf.level_samples(), 0, "clear empties the FIFO");
    // A take on the empty buffer is a clean underrun of silence.
    let out = buf.take_boundary_chunk(1600);
    assert_eq!(
        out.len(),
        0,
        "no channels seen yet after clear → empty chunk"
    );
}
