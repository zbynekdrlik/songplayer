//! Mutation-killing unit tests for `pacer.rs` (release PR #153).
//!
//! Each test PASSES on the current unmutated code and FAILS under exactly the
//! `cargo-mutants` mutation named above it. Every expected value is computed by
//! hand against the real source and its `sp_core::genlock` /
//! [`AudioGridBuffer`](crate::playback::audio_grid::AudioGridBuffer) /
//! [`AudioPll`](sp_core::genlock::audio::AudioPll) dependencies. Private helpers,
//! fields and methods are reached directly from this child module (`use
//! super::*`), asserting exact values at the precise input where the mutated
//! operator/constant diverges.
//!
//! Equivalent / unkillable mutants (deliberately NOT tested — see the task
//! report):
//!   * `pacer.rs:682:28` `+= -> *=` / `-=` — the `emit_now < boundary` branch in
//!     `service_standby` is dead: there is no decode pull between the scheduling
//!     read and the emit read, so `emit_now == sched_now >= boundary` always,
//!     and the branch body never runs.
//!   * `pacer.rs:731:20` `> -> >=` in `on_emit` — the body is only
//!     `max_late_us = late_us`; at `late_us == max_late_us` the assignment is a
//!     no-op, so `>` and `>=` are observationally identical.
//!   * `pacer.rs:788:70` `- -> +` / `- -> /` in `jitter_p99_us` — the fixed 99th
//!     percentile makes `floor(len*99/100) <= len-1 < len < len+1` for every
//!     `len`, so the `.min(len-1)` clamp never binds and the `len-1` operand can
//!     be `len+1` or `len/1` with no change.

use super::*;
use crate::playback::wallclock::{SettableClock, WallClock};
use sp_ndi::AudioFrame;

/// The first grid boundary after 0 — `wall_start` after `anchor()` at clock 0.
const B1: i64 = 333_333;

/// The k-th exact-rational grid boundary at 30 fps, in 100-ns units.
fn b(k: i64) -> i64 {
    k * 10_000_000 / 30
}

fn mk_frame(pts_ns: i64) -> PacedFrame {
    PacedFrame {
        pts_ns,
        width: 4,
        height: 2,
        stride: 4,
        video: vec![0u8; 12],
        audio: vec![],
    }
}

/// A frame whose presentation time lands exactly on `target_100ns` given a
/// `wall_start` of [`B1`] (i.e. `anchor()` was called at clock 0).
fn frame_due_at(target_100ns: i64) -> PacedFrame {
    mk_frame((target_100ns - B1) * 100)
}

/// Round exact integer-valued `f32`s to `i64`, so audio content is asserted
/// without float comparison.
fn as_ints(v: &[f32]) -> Vec<i64> {
    v.iter().map(|&x| x.round() as i64).collect()
}

/// Minimal recording sink: only the video stamp is read back.
#[derive(Default)]
struct RecordingSink {
    video_tcs: Vec<i64>,
}

impl PacedSink for RecordingSink {
    fn emit(
        &mut self,
        _video: &PacedFrame,
        _audio: &[AudioFrame],
        video_tc_100ns: i64,
        _audio_tc_100ns: i64,
    ) {
        self.video_tcs.push(video_tc_100ns);
    }
}

/// A pacer over a settable clock, anchored at clock 0 (wall_start = [`B1`]).
fn anchored_pacer() -> (Pacer, SettableClock) {
    let (wall, clk) = WallClock::settable(0);
    let mut pacer = Pacer::with_wallclock(30, true, wall);
    clk.set(0);
    pacer.anchor();
    (pacer, clk)
}

// ---------------------------------------------------------------------------
// level_avg_window (line 50): grid_fps.max(0) * 60
// ---------------------------------------------------------------------------

// Kills 50:5 (->0), 50:5 (->1), 50:22 (* -> +), 50:22 (* -> /).
#[test]
fn level_avg_window_is_grid_fps_times_sixty() {
    // 30 * 60 = 1800; distinguishes 1800 from 0, 1, 90 (+), 0 (/).
    assert_eq!(level_avg_window(30), 1800);
    // 60 * 60 = 3600; a second point pins the `*` shape further.
    assert_eq!(level_avg_window(60), 3600);
}

// ---------------------------------------------------------------------------
// plan_sleep_100ns (line 189): relatch = interval > 0 && delta > interval + 2
// ---------------------------------------------------------------------------

// Kills 189:46 (> -> >=): the `delta > interval + 2` comparison.
#[test]
fn plan_sleep_relatch_bound_is_strict_at_interval_plus_two() {
    let interval = 333_333;
    // delta == interval + 2 exactly: `>` gives no relatch, `>=` would relatch.
    let d = plan_sleep_100ns(0, interval + 2, interval);
    assert!(
        !d.relatch,
        "delta == interval + 2 is NOT a backward jump (strict >)"
    );
    assert_eq!(d.sleep_100ns, interval + 2);
}

// Kills 189:63 (+ -> *): the `interval + 2` bound.
#[test]
fn plan_sleep_relatch_bound_is_interval_plus_two_not_times_two() {
    let interval = 333_333;
    // delta in (interval+2, interval*2): `+` relatches, `*` (bound 666_666) does not.
    let d = plan_sleep_100ns(0, 400_000, interval);
    assert!(
        d.relatch,
        "delta 400_000 > interval+2 (333_335) is a backward jump; the bound is +2 not *2"
    );
}

// ---------------------------------------------------------------------------
// latched_boundary_100ns (line 212): nb > floor(now) + interval
// ---------------------------------------------------------------------------

// Kills 212:32 (> -> >=).
#[test]
fn latched_boundary_keeps_pending_at_exactly_floor_plus_interval() {
    let now = 4_666_666i64; // on-grid, followed by a 333_334-wide slot
    let interval = sp_core::genlock::interval_100ns(30);
    let floor = sp_core::genlock::floor_boundary_100ns(now, 30);
    let nb = floor + interval; // exactly floor(now) + interval
    // The next real grid boundary is the WIDE slot, so `>` and `>=` diverge.
    assert_ne!(
        sp_core::genlock::strict_next_boundary_100ns(now, 30),
        nb,
        "test needs a wide slot so `>` and `>=` diverge"
    );
    // Original `>`: nb is NOT strictly greater than floor+interval -> keep nb.
    // Mutant `>=`: would re-latch to strict_next(now).
    assert_eq!(
        latched_boundary_100ns(now, nb, 30),
        nb,
        "nb == floor(now)+interval must be KEPT, not re-latched"
    );
}

// ---------------------------------------------------------------------------
// with_wallclock (line 339): audio target = spb * AUDIO_TARGET_BOUNDARIES
// ---------------------------------------------------------------------------

// Kills 339:69 (* -> +) and (* -> /).
#[test]
fn with_wallclock_audio_target_is_spb_times_two_boundaries() {
    let (pacer, _clk) = anchored_pacer();
    // spb = 48000/30 = 1600, AUDIO_TARGET_BOUNDARIES = 2 -> 3200.
    // `+` would give 1602, `/` would give 800.
    assert_eq!(pacer.audio_buf.target_level(), 3200);
}

// ---------------------------------------------------------------------------
// interval_100ns getter (line 355)
// ---------------------------------------------------------------------------

// Kills 355:9 (->0), (-> -1), (->1).
#[test]
fn interval_100ns_returns_the_grid_interval() {
    let (pacer, _clk) = anchored_pacer();
    assert_eq!(pacer.interval_100ns(), 333_333);
}

// ---------------------------------------------------------------------------
// tick_wall (line 368)
// ---------------------------------------------------------------------------

// Kills 368:9 (replace body with ()).
#[test]
fn tick_wall_advances_the_resample_counter() {
    let (mut pacer, _clk) = anchored_pacer();
    assert_eq!(pacer.wall.frames_since_resample(), 0);
    pacer.tick_wall();
    assert_eq!(
        pacer.wall.frames_since_resample(),
        1,
        "tick_wall must forward to WallClock::tick"
    );
}

// ---------------------------------------------------------------------------
// has_pending (line 372)
// ---------------------------------------------------------------------------

// Kills 372:9 (->false) and (->true).
#[test]
fn has_pending_reflects_the_pending_slot() {
    let (mut pacer, _clk) = anchored_pacer();
    assert!(!pacer.has_pending(), "fresh anchor has no pending"); // kills ->true
    pacer.pending = Some(mk_frame(0));
    assert!(pacer.has_pending(), "a parked frame -> has_pending"); // kills ->false
}

// ---------------------------------------------------------------------------
// service (line 457): lag > GENLOCK_MAX_CATCHUP_INTERVALS
// ---------------------------------------------------------------------------

// Kills 457:16 (> -> >=).
#[test]
fn service_lag_exactly_eight_does_not_arm_the_reanchor_timer() {
    let (mut pacer, clk) = anchored_pacer(); // next_boundary = b(1)
    let mut sink = RecordingSink::default();
    let mut f = Some(frame_due_at(b(1)));
    // floor(now) = b(9) = 3_000_000; lag = (3_000_000 - 333_333)/333_333 = 8.
    clk.set(b(9));
    pacer.service(|| f.take(), &mut sink);
    assert!(
        pacer.lag_exceeded_since.is_none(),
        "lag == 8 is NOT over the catch-up bound (strict >); the timer must not arm"
    );
}

// ---------------------------------------------------------------------------
// service (line 459): elapsed > LAG_REANCHOR_AFTER_100NS
// ---------------------------------------------------------------------------

// Kills 459:48 (> -> >=).
#[test]
fn service_reanchor_needs_strictly_over_one_second_of_sustained_lag() {
    let (mut pacer, clk) = anchored_pacer();
    let mut sink = RecordingSink::default();

    // Call 1 at b(20): lag = 19 > 8 -> arm the sustain timer at since = b(20).
    let mut f1 = Some(frame_due_at(b(1)));
    clk.set(b(20)); // 6_666_666
    assert_eq!(
        pacer.service(|| f1.take(), &mut sink),
        ServiceOutcome::Emitted
    );
    assert_eq!(pacer.stats().resyncs, 0);

    // Call 2 exactly LAG_REANCHOR_AFTER_100NS (1 s) later: b(50) = 16_666_666,
    // and 16_666_666 - 6_666_666 = 10_000_000 == the sustain bound exactly.
    let mut f2 = Some(frame_due_at(b(2)));
    clk.set(b(50));
    let out = pacer.service(|| f2.take(), &mut sink);
    assert_eq!(
        out,
        ServiceOutcome::Emitted,
        "elapsed == 1 s is NOT strictly over the sustain gate; must catch up, not re-anchor"
    );
    assert_eq!(
        pacer.stats().resyncs,
        0,
        "no re-anchor at exactly the 1 s threshold (strict >)"
    );
}

// ---------------------------------------------------------------------------
// service (line 532): relatches += 1 on a backward step during decode
// ---------------------------------------------------------------------------

// Kills 532:28 (+= -> *=) and (+= -> -=).
#[test]
fn service_backward_clock_during_decode_relatches_once() {
    let (mut pacer, clk) = anchored_pacer();
    let clk2 = clk.clone();
    let mut sink = RecordingSink::default();
    let mut f = Some(frame_due_at(b(1)));
    clk.set(b(1)); // sched_now == boundary b(1)
    // The pull steps the clock BACKWARD before the emit read, so emit_now < boundary.
    let out = pacer.service(
        || {
            clk2.set(0);
            f.take()
        },
        &mut sink,
    );
    assert!(
        matches!(out, ServiceOutcome::Wait { .. }),
        "a backward step during decode re-latches (never emits future-dated)"
    );
    assert_eq!(
        pacer.stats().relatches,
        1,
        "exactly one backward-during-decode relatch (+= 1)"
    );
}

// ---------------------------------------------------------------------------
// service (line 550): queue_had_frame = had_frame || pending.is_some()
// ---------------------------------------------------------------------------

// Kills 550:41 (|| -> &&).
#[test]
fn service_buffered_frame_catches_up_never_resyncs() {
    let (mut pacer, clk) = anchored_pacer();
    let mut sink = RecordingSink::default();
    // lag = 13 > 8 with a frame consumed (had_frame = true) and nothing left
    // pending (pending = None): `||` -> queue_had_frame true (catch up one slot);
    // `&&` -> false (resync-leap to floor(now)).
    let mut f = Some(frame_due_at(b(1)));
    clk.set(b(14)); // 4_666_666, fresh lag (not sustained) -> no re-anchor this call
    let out = pacer.service(|| f.take(), &mut sink);
    assert_eq!(out, ServiceOutcome::Emitted);
    assert_eq!(
        pacer.stats().resyncs,
        0,
        "a buffered frame catches up one slot; `||` keeps queue_had_frame true so no resync"
    );
    assert_eq!(
        sink.video_tcs,
        vec![b(1)],
        "stamp is the caught-up boundary b(1), not a resync leap to floor(now)"
    );
}

// ---------------------------------------------------------------------------
// service_standby (line 668): next_boundary != 0 && boundary < next_boundary
// (line 669): relatches += 1 ; (line 671): sched_now < boundary
// ---------------------------------------------------------------------------

// Kills 668:42 (&& -> ||), 668:54 (< -> <=), 668:54 (< -> ==).
#[test]
fn service_standby_on_grid_step_is_not_a_relatch() {
    let (mut pacer, clk) = anchored_pacer();
    let mut sink = RecordingSink::default();
    let black = mk_frame(0);
    clk.set(b(1)); // sched_now == next_boundary == latched boundary (boundary == nb)
    let out = pacer.service_standby(Standby::Black(&black), &mut sink);
    assert_eq!(out, ServiceOutcome::Emitted);
    assert_eq!(
        pacer.stats().relatches,
        0,
        "boundary == next_boundary is not a relatch (strict <, real &&)"
    );
}

// Kills 668:37 (!= -> ==), 668:54 (< -> >), 669:28 (+= -> *=) and (+= -> -=),
// 671:22 (< -> >).
#[test]
fn service_standby_backward_step_relatches_once() {
    let (wall, clk) = WallClock::settable(100 * b(1));
    let mut pacer = Pacer::with_wallclock(30, true, wall);
    clk.set(b(100));
    pacer.anchor(); // next_boundary = strict_next(b(100))
    let mut sink = RecordingSink::default();
    let black = mk_frame(0);
    // Backward step far below the latched standby boundary.
    clk.set(b(50));
    let out = pacer.service_standby(Standby::Black(&black), &mut sink);
    assert!(
        matches!(out, ServiceOutcome::Wait { .. }),
        "a backward standby step waits, never emits a future-dated frame"
    );
    assert_eq!(
        pacer.stats().relatches,
        1,
        "a backward standby step re-latches exactly once"
    );
}

// ---------------------------------------------------------------------------
// service_standby (line 679): emit_now < boundary
// ---------------------------------------------------------------------------

// Kills 679:21 (< -> >).
#[test]
fn service_standby_services_a_late_boundary_by_emitting() {
    let (mut pacer, clk) = anchored_pacer();
    let mut sink = RecordingSink::default();
    let black = mk_frame(0);
    // First on-grid standby advances next_boundary to b(2).
    clk.set(b(1));
    assert_eq!(
        pacer.service_standby(Standby::Black(&black), &mut sink),
        ServiceOutcome::Emitted
    );
    // Late standby step: sched_now (b(5)) > boundary (b(2)); `emit_now < boundary`
    // is false, so the boundary is serviced by an emit. `emit_now > boundary`
    // would instead re-latch and Wait.
    clk.set(b(5));
    assert_eq!(
        pacer.service_standby(Standby::Black(&black), &mut sink),
        ServiceOutcome::Emitted,
        "a late standby boundary is emitted, never a Wait"
    );
}

// ---------------------------------------------------------------------------
// on_emit (line 728): late_100ns > LATE_THRESHOLD_100NS
// ---------------------------------------------------------------------------

// Kills 728:23 (> -> >=).
#[test]
fn on_emit_late_threshold_is_strict_at_two_ms() {
    let (mut pacer, _clk) = anchored_pacer();
    // late_100ns == LATE_THRESHOLD_100NS (20_000 == 2 ms) exactly.
    pacer.on_emit(20_000, 0);
    assert_eq!(
        pacer.stats().late_frames,
        0,
        "exactly 2 ms late is NOT strictly over the 2 ms late threshold"
    );
}

// ---------------------------------------------------------------------------
// iter_percentile_us (line 752): idx = ((len * p) / 100).min(len - 1)
// ---------------------------------------------------------------------------

// Kills 752:35 (* -> +), 752:40 (/ -> *), 752:65 (- -> +), 752:65 (- -> /).
#[test]
fn iter_percentile_index_math() {
    let (mut pacer, _clk) = anchored_pacer();
    for x in 1..=100u64 {
        pacer.push_iter(x);
    }
    assert_eq!(pacer.iter_len, 100);
    // p=50: idx = (100*50)/100 = 50 -> v[50] = 51.
    // `* -> +`: (100+50)/100 = 1 -> v[1] = 2. `/ -> *`: min(big,99) = 99 -> v[99] = 100.
    assert_eq!(pacer.iter_percentile_us(50), 51);
    // p=100: idx = min((100*100)/100, 99) = 99 -> v[99] = 100.
    // `- -> +`: min(100, 101) = 100 -> v[100] out of bounds (panic).
    // `- -> /`: min(100, 100) = 100 -> v[100] out of bounds (panic).
    assert_eq!(pacer.iter_percentile_us(100), 100);
}

// ---------------------------------------------------------------------------
// max_lag_slots getter (line 769)
// ---------------------------------------------------------------------------

// Kills 769:9 (->0), (-> -1), (->1).
#[test]
fn max_lag_slots_getter_returns_the_field() {
    let (mut pacer, _clk) = anchored_pacer();
    pacer.max_lag_slots = 5;
    assert_eq!(pacer.max_lag_slots(), 5);
}

// ---------------------------------------------------------------------------
// push_jitter (line 774): jitter_idx = (jitter_idx + 1) % JITTER_RING
// ---------------------------------------------------------------------------

// Kills 774:44 (+ -> *) and 774:49 (% -> /).
#[test]
fn push_jitter_advances_the_ring_index_by_one() {
    let (mut pacer, _clk) = anchored_pacer();
    assert_eq!(pacer.jitter_idx, 0);
    pacer.push_jitter(42);
    // (0 + 1) % 256 = 1. `+ -> *`: (0*1)%256 = 0. `% -> /`: (0+1)/256 = 0.
    assert_eq!(
        pacer.jitter_idx, 1,
        "push_jitter advances the ring index by exactly one"
    );
}

// ---------------------------------------------------------------------------
// jitter_p99_us (line 788): idx = ((len * 99) / 100).min(len - 1)
// ---------------------------------------------------------------------------

// Kills 788:37 (* -> +), 788:37 (* -> /), 788:43 (/ -> %), 788:43 (/ -> *).
#[test]
fn jitter_p99_index_math() {
    let (mut pacer, _clk) = anchored_pacer();
    for x in 1..=200u64 {
        pacer.push_jitter(x);
    }
    assert_eq!(pacer.jitter_len, 200);
    // idx = min((200*99)/100, 199) = min(198, 199) = 198 -> v[198] = 199.
    // `* -> +`: (200+99)/100 = 2 -> v[2] = 3. `* -> /`: (200/99)/100 = 0 -> v[0] = 1.
    // `/ -> %`: (200*99)%100 = 0 -> v[0] = 1. `/ -> *`: min(big,199) = 199 -> v[199] = 200.
    assert_eq!(pacer.jitter_p99_us(), 199);
}

// ---------------------------------------------------------------------------
// push_audio (line 819): if ch == 0 || af.data.is_empty()
// ---------------------------------------------------------------------------

// Kills 819:24 (|| -> &&).
#[test]
fn push_audio_skips_a_zero_channel_frame() {
    let (mut pacer, _clk) = anchored_pacer();
    // channels == 0 but non-empty data: `||` skips it; `&&` would fall through
    // to `data.len() / ch` = divide-by-zero (panic).
    let af = AudioFrame {
        data: vec![0.1, 0.2],
        channels: 0,
        sample_rate: 48_000,
        timecode_100ns: None,
    };
    pacer.push_audio(&[af]);
    assert_eq!(
        pacer.audio_buf.level_samples(),
        0,
        "a channels==0 frame is skipped; nothing is buffered"
    );
}

// ---------------------------------------------------------------------------
// push_audio (line 826): plane.push(af.data[j * ch + c])
// ---------------------------------------------------------------------------

// Kills 826:42 (* -> /) and 826:47 (+ -> *).
#[test]
fn push_audio_deinterleaves_interleaved_samples() {
    let (mut pacer, _clk) = anchored_pacer();
    // Stereo interleaved [L0,R0,L1,R1] = [10,20,30,40] -> ch0 [10,30], ch1 [20,40].
    let af = AudioFrame {
        data: vec![10.0, 20.0, 30.0, 40.0],
        channels: 2,
        sample_rate: 48_000,
        timecode_100ns: None,
    };
    pacer.push_audio(&[af]);
    let chunk = pacer.audio_buf.take_boundary_chunk(2);
    assert_eq!(chunk.len(), 2, "two channels");
    // `* -> /` (j/ch): ch0 = [10,10]. `+ -> *` ((j*ch)*c): ch0 = [10,10].
    assert_eq!(
        as_ints(&chunk[0]),
        vec![10, 30],
        "channel 0 = interleaved samples 0 and 2 (index j*ch + c)"
    );
    assert_eq!(
        as_ints(&chunk[1]),
        vec![20, 40],
        "channel 1 = interleaved samples 1 and 3"
    );
}

// ---------------------------------------------------------------------------
// take_boundary_audio (line 849): data[j * channels + c] = s
// ---------------------------------------------------------------------------

// Kills 849:24 (* -> +), 849:24 (* -> /), 849:35 (+ -> *).
#[test]
fn take_boundary_audio_reinterleaves_planar_samples() {
    let (mut pacer, _clk) = anchored_pacer();
    // Push a full boundary of planar audio directly: ch0 all 1.0, ch1 all 2.0.
    let spb = 1600usize;
    let ch0 = vec![1.0f32; spb];
    let ch1 = vec![2.0f32; spb];
    pacer.audio_buf.push(&[ch0, ch1]);
    let frames = pacer.take_boundary_audio();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].channels, 2);
    let d = &frames[0].data;
    assert_eq!(d.len(), spb * 2);
    // Interleaved back: [ch0_0, ch1_0, ch0_1, ch1_1, ...] = [1,2,1,2,...].
    // `* -> +`: indices start at 2, so d[0] stays 0. `* -> /`: d[2] becomes 2.
    // `+ -> *`: odd indices (d[1]) stay 0.
    assert_eq!(
        as_ints(&d[..4]),
        vec![1, 2, 1, 2],
        "re-interleave places ch c of sample j at index j*channels + c"
    );
}

// ---------------------------------------------------------------------------
// run_audio_control (line 882): if last_pll_100ns == 0  (seed)
// ---------------------------------------------------------------------------

// Kills 882:32 (== -> !=).
#[test]
fn run_audio_control_seeds_last_pll_on_the_first_call() {
    let (mut pacer, _clk) = anchored_pacer();
    assert_eq!(pacer.last_pll_100ns, 0, "fresh anchor");
    // now < the 60 s cadence: the else-if cannot fire, so only the `== 0` seed
    // can set last_pll. `!= 0` would take the (false) else-if and leave it 0.
    pacer.run_audio_control(5_000_000);
    assert_eq!(
        pacer.last_pll_100ns, 5_000_000,
        "the first run seeds last_pll at now (the `== 0` branch)"
    );
}

// ---------------------------------------------------------------------------
// run_audio_control (line 884): else if now - last_pll >= AUDIO_PLL_UPDATE_100NS
// ---------------------------------------------------------------------------

// Kills 884:29 (- -> +) and (- -> /).
#[test]
fn run_audio_control_cadence_uses_subtraction() {
    // `- -> +`: 585 ms elapsed is below the 600 ms cadence -> must NOT advance.
    let (mut p1, _c1) = anchored_pacer();
    p1.run_audio_control(10_000_000); // seed last_pll = 10_000_000
    p1.run_audio_control(595_000_000); // 585 ms elapsed
    assert_eq!(
        p1.last_pll_100ns, 10_000_000,
        "585 ms < 600 ms cadence: last_pll must not advance (subtraction, not +)"
    );
    // `- -> /`: 690 ms elapsed is over the cadence -> must advance.
    let (mut p2, _c2) = anchored_pacer();
    p2.run_audio_control(10_000_000);
    p2.run_audio_control(700_000_000); // 690 ms elapsed
    assert_eq!(
        p2.last_pll_100ns, 700_000_000,
        "690 ms >= 600 ms cadence: last_pll advances (subtraction, not /)"
    );
}

// Kills 884:51 (>= -> <).
#[test]
fn run_audio_control_cadence_is_inclusive_at_the_interval() {
    let (mut pacer, _clk) = anchored_pacer();
    pacer.run_audio_control(10_000_000); // seed
    // Exactly AUDIO_PLL_UPDATE_100NS (600 ms = 600_000_000) later.
    pacer.run_audio_control(610_000_000);
    assert_eq!(
        pacer.last_pll_100ns, 610_000_000,
        "elapsed == 600 ms cadence is inclusive (>=): last_pll advances"
    );
}

// ---------------------------------------------------------------------------
// audio_overflow_warn_needed (line 930)
// ---------------------------------------------------------------------------

// Kills 930:9 (->false) and (->true).
#[test]
fn audio_overflow_warn_needed_reflects_the_buffer() {
    let (mut pacer, _clk) = anchored_pacer();
    // No overflow yet -> false (kills the ->true mutant).
    assert!(
        !pacer.audio_overflow_warn_needed(),
        "no overflow yet -> no warn"
    );

    // Force an overflow past the 2 s cap (48000 * 2 = 96000 samples/channel).
    let big = vec![0.0f32; 100_000];
    pacer.audio_buf.push(&[big.clone(), big]);
    assert!(
        pacer.audio_buf.overflows() > 0,
        "pushing over the 2 s cap overflows"
    );
    // First call after an overflow -> true (kills the ->false mutant).
    assert!(
        pacer.audio_overflow_warn_needed(),
        "first call after overflow -> warn needed"
    );
    // One-shot per song.
    assert!(
        !pacer.audio_overflow_warn_needed(),
        "the overflow warning is one-shot"
    );
}

// ---------------------------------------------------------------------------
// Empty-ring percentile guards (#156): jitter_p99_us / iter_percentile_us each
// open with `if <len> == 0 { return 0 }`. Without the guard, `len - 1` on an
// empty ring panics either way — in debug (this test profile) on subtraction
// overflow, in release by wrapping to usize::MAX so `.min` never clamps and
// `v[huge]` indexes past an empty vec (the 0xc0000409 abort mechanism #156 was
// filed for). The index-math tests above all use full rings (len=100/200);
// these lock the untested empty case so a future refactor can't drop the guard.
// ---------------------------------------------------------------------------

#[test]
fn jitter_p99_us_on_empty_ring_returns_zero_without_panicking() {
    let (pacer, _clk) = anchored_pacer();
    assert_eq!(pacer.jitter_len, 0);
    assert_eq!(
        pacer.jitter_p99_us(),
        0,
        "an empty jitter ring must return 0, never index-panic"
    );
}

#[test]
fn iter_percentile_us_on_empty_ring_returns_zero_without_panicking() {
    let (pacer, _clk) = anchored_pacer();
    assert_eq!(pacer.iter_len, 0);
    for p in [0usize, 50, 99, 100] {
        assert_eq!(
            pacer.iter_percentile_us(p),
            0,
            "an empty iter ring must return 0 for p={p}, never index-panic"
        );
    }
    assert_eq!(pacer.iter_p50_us(), 0);
    assert_eq!(pacer.iter_p99_us(), 0);
}
