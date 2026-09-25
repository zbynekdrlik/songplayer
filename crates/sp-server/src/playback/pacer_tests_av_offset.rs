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

use super::{
    Rec, SyncedSource, anchored, assert_exact_stream, b, first_media, run_synced, samples_of,
    step_ahead,
};

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

// ---------------------------------------------------------------------------
// #148 v6 (Approach 3, ROZHODNUTÉ 5834440419): on the first FRESH frame of a
// new wall↔media map the pacer lands the picture origin on the grid,
// `wall_start = due(pts₀) − pts₀`, so that frame presents exactly on the
// boundary it is due at and is still shown there. The audio keeps its anchor
// `(pts₀, due(pts₀))`, which now IS the picture's line. Any seek / start
// position therefore reads the on-grid sawtooth: 30 fps reads 0, 24/25 fps
// read min 0 / mean 16.7 ms, whatever the landing phase.
// ---------------------------------------------------------------------------

/// A seek landing whose first frame would present at `b(1) + FIRST_PTS` =
/// 241.0 ms, i.e. 25.67 ms BEFORE `b(8)` (266.67 ms) — the box's SP-slow case
/// (#148 v5 telemetry: min −25.7 ms on a 23.976 fps song).
const FIRST_PTS: i64 = 2_076_667;

/// The same landing exactly ON `b(8)`: `b(1) + ON_GRID_PTS = b(8)`.
const ON_GRID_PTS: i64 = 2_333_333;

/// The boundary the landing is due at, and shown at: `b(8)`.
const LANDING: i64 = 8;

/// The grid boundary a frame presenting at `present` is due at (the first
/// boundary at or after it), as a boundary index.
fn due_index(present: i64) -> i64 {
    (0..).find(|&k| b(k) >= present).expect("a boundary exists")
}

/// The reading of minute 0 with the picture origin landed on the grid: the
/// first frame is due at `b(d)`, the origin becomes `b(d) − pts₀`, and frame
/// `j` presents at `b(d) + (pts_j − pts₀)`. The block at `b(k)` starts at the
/// audio anchor line `samples(pts₀) + samples(b(k) − b(d))`; the frame handed
/// with it is the newest that has presented. Boundaries before `b(d)` read
/// nothing (no frame has presented before `b(d)`).
fn on_grid_minute_0(pts: fn(i64) -> i64) -> Vec<f64> {
    let pts0 = pts(0);
    let due = b(due_index(b(1) + pts0));
    let origin = due - pts0;
    let mut out = Vec::new();
    let mut next = 0i64;
    let mut shown: Option<i64> = None;
    for k in 1..=LAST_OF_MINUTE_0 {
        while origin + pts(next) <= b(k) {
            shown = Some(pts(next));
            next += 1;
        }
        if let Some(p) = shown {
            let media = samples_of(pts0) + samples_of(b(k) - due);
            out.push(media as f64 * 1000.0 / 48_000.0 - p as f64 / 10_000.0);
        }
    }
    out
}

/// Play minute 0 of `pts` (audio contiguous from media 0), then read the stats
/// in minute 1 before any of its boundaries was serviced.
fn play_minute_0(pts: fn(i64) -> i64) -> (crate::playback::pacer::Pacer, Rec) {
    let (mut pacer, clk) = anchored();
    let src = RefCell::new(SyncedSource::new(pts, 0));
    let mut rec = Rec::default();
    run_synced(&mut pacer, &clk, &src, 1, LAST_OF_MINUTE_0, &mut rec);
    clk.set(b(LAST_OF_MINUTE_0 + 6));
    (pacer, rec)
}

/// The shared checks of a landing at `b(LANDING)`: shown there, every shown
/// boundary measured, only the start drop, the picture origin on the grid,
/// and the reported minute equal to the on-grid model. Returns the model's
/// `(mean, min, max)` and the recorded blocks.
fn assert_landed_on_grid(pts: fn(i64) -> i64, what: &str) -> ((f64, f64, f64), Rec) {
    let (pacer, rec) = play_minute_0(pts);
    assert_eq!(
        rec.blocks[0].0,
        b(LANDING),
        "{what}: first frame shown at b(8)"
    );
    assert_eq!(
        pacer.wall_start_100ns,
        b(LANDING) - pts(0),
        "{what}: the picture origin lands the first frame on b(8)"
    );
    let want = summary(&on_grid_minute_0(pts));
    let readings = pacer.av.frame_offset.readings(pacer.now_100ns());
    assert_eq!(readings, 1792, "{what}: b(8..=1799) were measured");
    assert_eq!(
        pacer.stats().av_corrections,
        1,
        "{what}: only the start drop — the audio stays on the picture line"
    );
    let got = reported(&pacer);
    assert!(
        got.1.abs() <= ONE_SAMPLE_MS,
        "{what}: min {} ms — the audio must not run late by the landing's 25.7 ms phase",
        got.1
    );
    assert_close(got, want, 1e-6, what);
    (want, rec)
}

#[test]
fn a_23_976_fps_seek_landing_25_7_ms_before_a_boundary_reads_min_0_not_minus_25_7() {
    // 23.976 fps with the decoder's integer-ms pts. The v5 code (origin off
    // the grid, audio on the due line) read this minute as −5.05 / −25.67 /
    // +16.0 — the box's SP-slow reading. With the origin landed on the grid it
    // is the on-grid 23.976 sawtooth: min 0, max ≈ one source frame interval
    // (41.7 ms), mean 20.7 ms (the 23.976/30 phase drifts within the minute).
    let (want, _) = assert_landed_on_grid(|j| FIRST_PTS + (j * 1001 / 24) * 10_000, "23.976 fps");
    assert!((want.2 - 125.0 / 3.0).abs() < 1e-3, "max {}", want.2);
}

#[test]
fn exact_24_and_25_fps_seek_landings_read_the_on_grid_sawtooth() {
    // After the landing every 4th (24 fps) / 5th (25 fps) frame presents on a
    // boundary, so the minute reads the analytic on-grid sawtooth: min 0,
    // max one grid slot, mean 100/6 ms. The v5 code read it 25.67 ms lower
    // (24 fps min −25.0, 25 fps min −20.0).
    let cases: [(fn(i64) -> i64, &str); 2] = [
        (|j: i64| FIRST_PTS + j * 10_000_000 / 24, "24 fps"),
        (|j: i64| FIRST_PTS + j * 400_000, "25 fps"),
    ];
    for (pts, what) in cases {
        let (want, _) = assert_landed_on_grid(pts, what);
        assert!(want.1.abs() < 1e-3, "{what}: analytic min {}", want.1);
        assert!(
            (want.2 - 100.0 / 3.0).abs() < 1e-3,
            "{what}: max {}",
            want.2
        );
        assert!(
            (want.0 - 100.0 / 6.0).abs() < 0.01,
            "{what}: analytic mean {}",
            want.0
        );
    }
}

#[test]
fn a_30_fps_seek_landing_off_the_grid_reads_exactly_0() {
    // 30 fps content landing 25.67 ms before b(8): every frame is shown at its
    // due boundary, and the audio block handed with it starts at that frame's
    // own media time — bit-exact consecutive samples from samples(pts₀) = 9968.
    let pts: fn(i64) -> i64 = |j| FIRST_PTS + b(j);
    let (want, rec) = assert_landed_on_grid(pts, "30 fps");
    for v in [want.0, want.1, want.2] {
        assert!(v.abs() <= ONE_SAMPLE_MS, "30 fps analytic reading {v}");
    }
    assert_eq!(
        first_media(&rec.blocks[0].1),
        9968,
        "the frame's media time"
    );
    assert_exact_stream(&rec.blocks, 9968);
}

#[test]
fn an_on_grid_seek_landing_is_unchanged() {
    // The first frame presents exactly on b(8): the origin is already on the
    // grid and nothing moves. The first block starts at the frame's own media
    // time (233.3 ms = 11200 samples), 24 fps plays bit-exact, and the minute
    // reads the aligned sawtooth.
    let pts: fn(i64) -> i64 = |j| ON_GRID_PTS + j * 10_000_000 / 24;
    let (want, rec) = assert_landed_on_grid(pts, "on-grid 24 fps");
    assert!(want.1.abs() < 1e-3 && (want.0 - 100.0 / 6.0).abs() < 0.01);
    assert_eq!(
        first_media(&rec.blocks[0].1),
        11_200,
        "the frame's media time"
    );
    assert_exact_stream(&rec.blocks, 11_200);
}

#[test]
fn each_new_map_lands_its_origin_once_and_resume_keeps_it() {
    // The origin moves ONLY on the first fresh frame of a NEW map (anchor() on
    // play/seek, a lag re-anchor). A Resume keeps the map (resnap): moving the
    // origin onto a later, off-grid 24 fps frame would shift the picture off
    // the audio line.
    let (mut pacer, clk) = anchored();
    let before = RefCell::new(SyncedSource::new(|j| FIRST_PTS + j * 10_000_000 / 24, 0));
    let mut rec = Rec::default();
    run_synced(&mut pacer, &clk, &before, 1, 20, &mut rec);
    let landed = b(LANDING) - FIRST_PTS;
    assert_eq!(pacer.wall_start_100ns, landed, "the first landing");

    // Resume: frame 10 (b(8) + 416.7 ms, off the grid) is the first fresh
    // frame after it — the origin must not move onto it.
    pacer.audio_resume_reset();
    run_synced(&mut pacer, &clk, &before, 21, 40, &mut rec);
    assert_eq!(pacer.wall_start_100ns, landed, "Resume keeps the origin");

    // A seek is a new map: anchor() puts the origin on b(41) and the new
    // landing (present b(41) + 207.67 ms, due b(48)) moves it again.
    clk.set(b(40) + 100);
    pacer.anchor();
    let after = RefCell::new(SyncedSource::new(|j| FIRST_PTS + j * 10_000_000 / 24, 0));
    run_synced(&mut pacer, &clk, &after, 41, 60, &mut rec);
    assert_eq!(due_index(b(41) + FIRST_PTS), 48);
    assert_eq!(
        pacer.wall_start_100ns,
        b(48) - FIRST_PTS,
        "the seek's first frame lands on b(48)"
    );
}
