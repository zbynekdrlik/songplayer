//! #148 v5: `av_frame_offset` — SongPlayer's own emitted A/V relation.
//!
//! At every productive paced boundary the pacer measures
//! `audio_block_media_start − emitted_frame_pts` (ms): the media time of the
//! first audio sample handed to the sink minus the pts of the video frame
//! handed with it (a repeat boundary uses the repeated frame's pts). It keeps
//! mean / min / max per UTC minute of the boundary stamp and reports the last
//! COMPLETE minute on `PacingStats`, so the `ndi: genlock` line logged in
//! minute M carries the whole of minute M−1.
//!
//! The wall clock starts at 0, so UTC minute 0 holds the boundaries
//! `b(1)..=b(1799)` and `b(1800)` (exactly 60 s) opens minute 1.
//!
//! Nested under `pacer_tests_av_align.rs` so it reuses its media-encoded
//! frame helpers (`enc(s) = s + 1e6` per sample, `SyncedSource`,
//! `ahead_frame`).

use std::cell::{Cell, RefCell};

use super::{Rec, SyncedSource, anchored, b, run_synced, step_ahead};

/// One 48 kHz sample in milliseconds — the "± 1 sample" tolerance.
const ONE_SAMPLE_MS: f64 = 1000.0 / 48_000.0;

/// The last boundary of UTC minute 0 (`b(1800)` = 60 s is minute 1).
const LAST_OF_MINUTE_0: i64 = 1799;

/// `(mean, min, max)` of a list of readings.
fn summary(v: &[f64]) -> (f64, f64, f64) {
    let mean = v.iter().sum::<f64>() / v.len() as f64;
    let min = v.iter().copied().fold(f64::INFINITY, f64::min);
    let max = v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    (mean, min, max)
}

fn reported(pacer: &crate::playback::pacer::Pacer) -> (f64, f64, f64) {
    let s = pacer.stats();
    (
        s.av_frame_offset_ms,
        s.av_frame_offset_min_ms,
        s.av_frame_offset_max_ms,
    )
}

fn assert_close(got: (f64, f64, f64), want: (f64, f64, f64), tol: f64, what: &str) {
    let names = ["mean", "min", "max"];
    for (i, (g, w)) in [(got.0, want.0), (got.1, want.1), (got.2, want.2)]
        .into_iter()
        .enumerate()
    {
        assert!(
            (g - w).abs() <= tol,
            "{what}: av_frame_offset {} = {g}, want {w} (± {tol})",
            names[i]
        );
    }
}

#[test]
fn av_frame_offset_of_a_30_fps_source_aligned_from_0_reads_0_within_one_sample() {
    // Frame j at pts b(j), audio contiguous from media 0: every boundary hands
    // the sink frame j with the audio block that starts at media j·1600.
    let (mut pacer, clk) = anchored();
    let src = RefCell::new(SyncedSource::new(b, 0));
    let mut rec = Rec::default();
    run_synced(&mut pacer, &clk, &src, 1, LAST_OF_MINUTE_0 + 2, &mut rec);

    assert_eq!(rec.blocks.len() as i64, LAST_OF_MINUTE_0 + 2);
    assert_eq!(pacer.stats().av_corrections, 0, "aligned input");
    // A MEASURED 0, not an empty minute: every boundary of minute 0 was read.
    let readings = pacer.av.frame_offset.readings(pacer.now_100ns());
    assert_eq!(
        readings as i64, LAST_OF_MINUTE_0,
        "b(1..=1799) were measured"
    );
    assert_close(
        reported(&pacer),
        (0.0, 0.0, 0.0),
        ONE_SAMPLE_MS,
        "30 fps, minute 0",
    );
}

#[test]
fn av_frame_offset_of_a_25_fps_source_on_the_30_fps_grid_matches_the_analytic_sawtooth() {
    // 25-fps content: frame j at pts 40·j ms. The anchor is frame 0, due at
    // b(1) = wall_start. Boundary b(1 + m) therefore carries the anchor-line
    // audio from media m·1600 samples = m·100/3 ms, and the NEWEST frame whose
    // present time b(1) + 40·j ms is ≤ that boundary: j = ⌊5m/6⌋. So the
    // reading is m·100/3 − 40·⌊5m/6⌋ ms: 0, 33.3, 26.7, 20, 13.3, 6.7, 0, …
    // (the picture is held on the grid while the audio runs on the line).
    let (mut pacer, clk) = anchored();
    let src = RefCell::new(SyncedSource::new(|j| j * 400_000, 0));
    let mut rec = Rec::default();
    run_synced(&mut pacer, &clk, &src, 1, LAST_OF_MINUTE_0, &mut rec);
    // Read the stats in minute 1 before any boundary of it was serviced (a
    // paused output reporting its last playing minute).
    clk.set(b(LAST_OF_MINUTE_0 + 6));

    let minute_0: Vec<f64> = (0..LAST_OF_MINUTE_0)
        .map(|m| m as f64 * 100.0 / 3.0 - 40.0 * ((5 * m) / 6) as f64)
        .collect();
    let want = summary(&minute_0);
    // The derivation itself: a 0 … 33.3 ms sawtooth with a mean near 100/6.
    assert!(want.1.abs() < 1e-9 && (want.2 - 100.0 / 3.0).abs() < 1e-9);
    assert!((want.0 - 100.0 / 6.0).abs() < 0.01, "mean {}", want.0);

    assert_eq!(
        pacer.stats().av_corrections,
        0,
        "the sawtooth is not an error"
    );
    let readings = pacer.av.frame_offset.readings(pacer.now_100ns());
    assert_eq!(
        readings as i64, LAST_OF_MINUTE_0,
        "b(1..=1799) were measured"
    );
    assert_close(reported(&pacer), want, 1e-6, "25 fps, minute 0");
}

#[test]
fn an_injected_20_ms_audio_offset_reads_plus_20_and_the_window_resets_per_minute() {
    // A 30-fps source with exactly paired audio. After b(100) the test drops
    // 960 buffered samples (20 ms) — what a 2 s cap overflow does — so the b(101)
    // block starts 20 ms of media AHEAD of its frame: +20 ms. The continuous
    // correction then inserts 48 samples per block: +20, +19, …, +2 (19 blocks),
    // after which the 1 ms residual (+1 ms) stays inside the dead band.
    let (mut pacer, clk) = anchored();
    let next = Cell::new(0i64);
    let mut rec = Rec::default();
    for k in 1..=2 * (LAST_OF_MINUTE_0 + 1) + 5 {
        step_ahead(&mut pacer, &clk, b(k), &next, false, 0, &mut rec);
        if k == 100 {
            let (gone, _) = pacer.audio_buf.take_block(960, 0);
            assert_eq!(gone[0].len(), 960, "20 ms of audio injected away");
        }
        if k == LAST_OF_MINUTE_0 {
            // Minute 0 is still in progress: no complete minute to report.
            assert_eq!(reported(&pacer), (0.0, 0.0, 0.0), "no complete minute yet");
        }
        if k == LAST_OF_MINUTE_0 + 6 {
            // Minute 0: b(1..=100) read 0, b(101..=119) read 20 … 2, and
            // b(120..=1799) read 1.
            let mut minute_0 = vec![0.0; 100];
            minute_0.extend((2..=20).rev().map(|ms| ms as f64));
            minute_0.extend(std::iter::repeat_n(1.0, 1680));
            assert_eq!(minute_0.len() as i64, LAST_OF_MINUTE_0);
            let want = summary(&minute_0);
            assert_eq!(want.2, 20.0, "the injected boundary reads +20 ms");
            assert_close(reported(&pacer), want, 1e-3, "minute 0");
        }
    }
    // Minute 1 holds only the +1 ms residual: min/max/mean all read 1 — the
    // +20 reading of minute 0 does not leak into it.
    assert_close(reported(&pacer), (1.0, 1.0, 1.0), 1e-3, "minute 1");
    assert_eq!(rec.blocks.len() as i64, 2 * (LAST_OF_MINUTE_0 + 1) + 5);
}
