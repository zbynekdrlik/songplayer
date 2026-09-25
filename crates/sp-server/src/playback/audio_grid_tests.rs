//! `AudioGridBuffer` tests (#148) — the planar FIFO with a media-time head that
//! delivers exactly `samples_per_boundary` samples per grid boundary, the hard
//! start/seek alignment (`align_to`), and the continuous correction
//! (`correction_for` + `take_block`). `super::*` resolves to the `audio_grid`
//! module under test.

use super::*;

/// A one-channel planar chunk of a constant value.
fn const_chunk(value: f32, n: usize) -> Vec<Vec<f32>> {
    vec![vec![value; n]]
}

/// A one-channel ramp `start, start+1, …` (exact integer-valued `f32`s).
fn ramp(start: i64, n: usize) -> Vec<Vec<f32>> {
    vec![(0..n as i64).map(|i| (start + i) as f32).collect()]
}

/// 100-ns media time of sample `s` at 48 kHz (multiples of 48 samples only).
fn tc(s: i64) -> i64 {
    s / 48 * 10_000
}

#[test]
fn every_boundary_yields_exactly_the_requested_samples_and_level_stays_bounded() {
    // 23.976-fps source (2002 samples/frame) paced onto a 30-fps grid
    // (1600 samples/boundary): 300 boundaries (10 s), ~240 source frames.
    let mut buf = AudioGridBuffer::new(48_000);
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
    let mut buf = AudioGridBuffer::new(48_000);
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
    let mut buf = AudioGridBuffer::new(48_000);
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
fn a_corrected_drop_is_spread_over_the_block_by_linear_interpolation() {
    // 4 outputs from 6 inputs (extra +2): positions 0, 5/3, 10/3, 5.
    let mut buf = AudioGridBuffer::new(48_000);
    buf.push(&ramp(0, 10));
    let (out, applied) = buf.take_block(4, 2);
    assert_eq!(applied, 2);
    let want = [0.0f32, 5.0 / 3.0, 10.0 / 3.0, 5.0];
    for (j, (&got, &w)) in out[0].iter().zip(want.iter()).enumerate() {
        assert!((got - w).abs() < 1e-4, "output {j}: {got} vs {w}");
    }
    assert_eq!(buf.level_samples(), 4, "6 inputs consumed");
    // The next block starts exactly at input 6 — no gap, no repeat.
    assert_eq!(buf.take_boundary_chunk(2)[0], vec![6.0f32, 7.0]);
}

#[test]
fn a_corrected_insert_stretches_fewer_inputs_over_the_block() {
    // 4 outputs from 2 inputs (extra −2): positions 0, 1/3, 2/3, 1.
    let mut buf = AudioGridBuffer::new(48_000);
    buf.push(&ramp(10, 8));
    let (out, applied) = buf.take_block(4, -2);
    assert_eq!(applied, -2);
    let want = [10.0f32, 10.0 + 1.0 / 3.0, 10.0 + 2.0 / 3.0, 11.0];
    for (j, (&got, &w)) in out[0].iter().zip(want.iter()).enumerate() {
        assert!((got - w).abs() < 1e-4, "output {j}: {got} vs {w}");
    }
    assert_eq!(buf.level_samples(), 6, "only 2 inputs consumed");
}

#[test]
fn a_correction_needs_the_whole_input_span_buffered() {
    // Exactly n + extra buffered: the correction applies (and the last output
    // reads the last buffered sample, never one past it).
    let mut exact = AudioGridBuffer::new(48_000);
    exact.push(&ramp(0, 6));
    let (out, applied) = exact.take_block(4, 2);
    assert_eq!(applied, 2);
    assert_eq!(out[0][3], 5.0);
    assert_eq!(exact.level_samples(), 0);
    // One short: a plain bit-exact block instead, no underrun.
    let mut short = AudioGridBuffer::new(48_000);
    short.push(&ramp(0, 5));
    let (out, applied) = short.take_block(4, 2);
    assert_eq!(applied, 0);
    assert_eq!(out[0], vec![0.0f32, 1.0, 2.0, 3.0]);
    assert_eq!(short.underruns(), 0);
    assert_eq!(short.level_samples(), 1);
}

#[test]
fn a_zero_sample_take_returns_nothing_and_consumes_nothing() {
    let mut buf = AudioGridBuffer::new(48_000);
    buf.push(&ramp(0, 5));
    assert_eq!(buf.take_block(0, 0), (Vec::new(), 0));
    assert_eq!(buf.level_samples(), 5);
}

#[test]
fn underrun_plays_the_real_samples_and_advances_the_head_only_by_them() {
    let mut buf = AudioGridBuffer::new(48_000);
    buf.push_media(&ramp(480, 10), Some(tc(480)));
    let out = buf.take_boundary_chunk(16);
    assert_eq!(&out[0][..10], &ramp(480, 10)[0][..]);
    assert!(out[0][10..].iter().all(|&v| v == 0.0), "zero-filled tail");
    assert_eq!(buf.underruns(), 1);
    assert_eq!(
        buf.head_media(),
        Some(490),
        "only the 10 real samples count"
    );
}

#[test]
fn the_first_timed_push_fixes_the_head_and_later_pushes_only_count() {
    let mut buf = AudioGridBuffer::new(48_000);
    assert_eq!(buf.head_media(), None, "no head before a timed push");
    buf.push_media(&ramp(8640, 100), Some(1_800_000));
    assert_eq!(buf.head_media(), Some(8640), "180 ms = 8640 samples");
    // A later timestamp is ignored: the decoded audio is contiguous.
    buf.push_media(&ramp(8740, 50), Some(999_999_999));
    assert_eq!(buf.head_media(), Some(8640));
    buf.take_boundary_chunk(30);
    assert_eq!(
        buf.head_media(),
        Some(8670),
        "the head advances by the take"
    );
    assert_eq!(buf.channels(), 1);
}

#[test]
fn a_timed_push_after_untimed_audio_backs_the_head_up_by_the_level() {
    let mut buf = AudioGridBuffer::new(48_000);
    buf.push(&ramp(0, 100));
    assert_eq!(buf.head_media(), None);
    buf.push_media(&ramp(960, 50), Some(tc(960)));
    assert_eq!(
        buf.head_media(),
        Some(860),
        "the 100 untimed samples precede it"
    );
}

#[test]
fn align_to_drops_early_audio() {
    let mut buf = AudioGridBuffer::new(48_000);
    buf.push_media(&ramp(0, 5000), Some(0));
    assert_eq!(buf.align_to(1000), (true, 1000));
    assert_eq!(buf.head_media(), Some(1000));
    assert_eq!(buf.level_samples(), 4000);
    assert_eq!(buf.take_boundary_chunk(2)[0], vec![1000.0f32, 1001.0]);
}

#[test]
fn align_to_pads_late_audio_with_leading_silence() {
    let mut buf = AudioGridBuffer::new(48_000);
    buf.push_media(&ramp(2400, 100), Some(tc(2400)));
    assert_eq!(buf.align_to(0), (true, -2400));
    assert_eq!(buf.head_media(), Some(0));
    assert_eq!(buf.level_samples(), 2500);
    let block = buf.take_boundary_chunk(2402);
    assert!(
        block[0][..2400].iter().all(|&v| v == 0.0),
        "2400 samples of silence"
    );
    assert_eq!(&block[0][2400..], &[2400.0f32, 2401.0]);
}

#[test]
fn align_to_with_too_little_audio_drops_all_and_reports_not_aligned() {
    let mut buf = AudioGridBuffer::new(48_000);
    buf.push_media(&ramp(0, 500), Some(0));
    assert_eq!(buf.align_to(1000), (false, 500));
    assert_eq!(buf.head_media(), Some(500));
    assert_eq!(buf.level_samples(), 0);
}

#[test]
fn align_to_at_the_head_changes_nothing() {
    let mut buf = AudioGridBuffer::new(48_000);
    buf.push_media(&ramp(96, 10), Some(tc(96)));
    assert_eq!(buf.align_to(96), (true, 0));
    assert_eq!(buf.level_samples(), 10);
}

#[test]
fn align_to_never_pads_beyond_the_two_second_cap() {
    // rate 1000 → cap 2000. Pad + what is already buffered must fit the cap,
    // or the next push's cap trim would drain the fresh padding again.
    let mut over = AudioGridBuffer::new(1000);
    over.push_media(&ramp(1991, 10), Some(1991 * 10_000));
    assert_eq!(over.head_media(), Some(1991));
    assert_eq!(over.align_to(0), (false, 0), "1991 pad + 10 buffered > cap");
    assert_eq!(over.level_samples(), 10, "nothing touched");
    // Exactly the cap is allowed.
    let mut fits = AudioGridBuffer::new(1000);
    fits.push_media(&ramp(1990, 10), Some(1990 * 10_000));
    assert_eq!(fits.align_to(0), (true, -1990));
    assert_eq!(fits.level_samples(), 2000, "level never exceeds the cap");
}

#[test]
fn align_to_without_a_media_head_touches_nothing() {
    let mut buf = AudioGridBuffer::new(48_000);
    buf.push(&ramp(0, 100));
    assert_eq!(buf.align_to(50), (false, 0));
    assert_eq!(buf.level_samples(), 100);
}

#[test]
fn an_overflow_drop_advances_the_head() {
    // rate 1000 → cap 2000: pushing 3000 drops the oldest 1000.
    let mut buf = AudioGridBuffer::new(1000);
    buf.push_media(&ramp(0, 3000), Some(0));
    assert_eq!(buf.head_media(), Some(1000));
    assert_eq!(buf.take_boundary_chunk(2)[0], vec![1000.0f32, 1001.0]);
}

#[test]
fn correction_for_engages_past_5_ms_and_stops_at_1_ms() {
    // Idle: engages only past 240 samples (5 ms), either sign.
    assert_eq!(correction_for(240, false), (0, false));
    assert_eq!(correction_for(-240, false), (0, false));
    assert_eq!(
        correction_for(241, false),
        (-48, true),
        "audio ahead → insert"
    );
    assert_eq!(
        correction_for(-241, false),
        (48, true),
        "audio behind → drop"
    );
    assert_eq!(
        correction_for(-960, false),
        (48, true),
        "at most 48 per block"
    );
    // Engaged: keeps going past 48 samples (1 ms), stops at or below.
    assert_eq!(correction_for(49, true), (-48, true));
    assert_eq!(correction_for(-49, true), (48, true));
    assert_eq!(correction_for(48, true), (0, false));
    assert_eq!(correction_for(-48, true), (0, false));
    assert_eq!(correction_for(0, true), (0, false));
    // An idle controller ignores a 1–5 ms error.
    assert_eq!(correction_for(100, false), (0, false));
}

#[test]
fn samples_from_100ns_rounds_to_the_nearest_sample() {
    assert_eq!(samples_from_100ns(1_800_000, 48_000), 8640);
    assert_eq!(samples_from_100ns(-1_800_000, 48_000), -8640);
    // One 30-fps grid interval (333 333 × 100 ns) = 1599.998 → 1600.
    assert_eq!(samples_from_100ns(333_333, 48_000), 1600);
    // 104 × 100 ns = 0.4992 samples → 0; 105 → 0.504 → 1.
    assert_eq!(samples_from_100ns(104, 48_000), 0);
    assert_eq!(samples_from_100ns(105, 48_000), 1);
    assert_eq!(samples_from_100ns(10_000_000, 44_100), 44_100);
}

#[test]
fn overflow_warning_fires_once_per_song_and_rearms_on_clear() {
    let mut buf = AudioGridBuffer::new(48_000);
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
fn clear_empties_the_buffer_and_forgets_the_head() {
    let mut buf = AudioGridBuffer::new(48_000);
    buf.push_media(&const_chunk(1.0, 5000), Some(0));
    assert!(buf.level_samples() > 0);
    buf.clear();
    assert_eq!(buf.level_samples(), 0, "clear empties the FIFO");
    assert_eq!(buf.head_media(), None, "clear forgets the media head");
    assert_eq!(buf.channels(), 0);
    // A take on the empty buffer yields no chunk (no channels seen yet) and is
    // not an underrun (there is no stream to starve).
    let out = buf.take_boundary_chunk(1600);
    assert_eq!(
        out.len(),
        0,
        "no channels seen yet after clear → empty chunk"
    );
    assert_eq!(buf.underruns(), 0);
}

#[test]
fn a_stereo_push_fixes_two_channels() {
    let mut buf = AudioGridBuffer::new(48_000);
    assert_eq!(buf.channels(), 0);
    buf.push(&[vec![1.0, 2.0], vec![-1.0, -2.0]]);
    assert_eq!(buf.channels(), 2);
    let block = buf.take_boundary_chunk(2);
    assert_eq!(block, vec![vec![1.0f32, 2.0], vec![-1.0, -2.0]]);
}
