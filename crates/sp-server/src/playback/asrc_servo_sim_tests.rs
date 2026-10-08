//! #233: the servo in a closed loop. A card consumes `buffer` frames per
//! callback at `rate·(1 + card_ppm)`; the program hands one block per grid
//! slot, `h ∈ [0, jitter]` late (a seeded LCG, one draw per block), or by a
//! realistic hand-off profile (below); the card's callbacks due before that
//! hand-off run first; the servo's correction makes
//! `1600·rate/48000·(1 + ppm)` frames per block (fractional carry). A
//! re-centre goes through `frames_from_100ns` like the worker's: an insert
//! lands at once, a skip is taken from each later block's output (the splice
//! skips at most one block per call) and counted in the observation as
//! pending. The observation counts the splice's 5 ms hold as buffered, as
//! the worker does (`asio_out.rs`: the ring + `Splice::held_frames`), while
//! the card plays only the ring: its cushion is ~61.7 ms, not the target's
//! 66.7 (review round 2: without the hold the harness had 5 ms more cushion
//! than the worker, and fewer underruns). The card's underrun frames reach
//! the servo as the worker counts them: a whole buffer per short callback
//! once the first block primed the ring. A clock step shifts the program
//! timeline against the card at `step_at_s` (forward = the program catches
//! up, backward = it pauses); a dropped buffer is one callback the host
//! missed; `buffer_after` makes the card call back with another size from
//! `buffer_after.0`, with no reopen (a real driver's buffer-size change
//! resets it: the worker reopens and primes, no hard re-centre). 900 s per
//! case. The slew is checked on the 100 ns instants the servo saw (rounding
//! the float wall would let a 1e-7 s quantum read as a 5e-7 ppm over-move).
//!
//! The owner's ruling (8.10.2026, #233 comment 6053850076): the resampler
//! absorbs a difference by its ratio, never by a skip or an insert. So every
//! case below runs with **0 hard re-centres** — the measured hand-off (the
//! nominal case, at 128 and 512 frames), cards at ±50 ppm, ±1 ms steps and
//! a +33 ms step (a date step's forward remainder) at three window phases,
//! the pessimistic 10–33 ms hand-off with clumps (the stress case), a
//! callback period that changes mid-run, a dropped buffer, a 100 ms step, a
//! 150 ms worker stall at four phases of the slot — except the ring-limit
//! cases, which force exactly one: a missing boundary (−33 ms, under the
//! floor of the splice's hold + one slot since the main session's ruling,
//! #233 comment 6056680979, Q2), a 300 ms stall at four phases and a 100 ms
//! pause. The excess an underrun leaves is kept as cushion (Q1), up to one
//! slot. A stall near 200 ms sits at the ring's
//! limit: it leaves 4 slots over the target or just under, by phase (the
//! model: none at 500.0 / 500.011 / 500.022 / 500.033 s, one at 500.03 s),
//! so no test pins it. A +33 ms step is back within ±2 ms of the target in
//! at most 145.4 s (≤ 150 s asked): 60 s ramping to 300 ppm at 5 ppm/s, a
//! cruise, the stop curve. Every figure pinned here comes from a scratch
//! model of this file and the servo (`rust-workspace.md`, deriving pins),
//! whose run of the old cases matched their earlier pins exactly.

use super::*;

struct Lcg(u64);

impl Lcg {
    fn unit(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// How late each block is handed over.
#[derive(Clone, Copy)]
enum Lateness {
    /// Uniform over `[0, jitter_s]` (one draw).
    Uniform,
    /// SP-program's hand-off as MEASURED over 4 h (#233 comment 6056680979,
    /// 433 170 boundaries): p50 70 µs, p99 141 µs, p99.9 242 µs, max
    /// 9.36 ms, none over 10 ms — the nominal case. Uniform between the
    /// quantiles (two draws: the band, then the value; the half under the
    /// median from an assumed 30 µs).
    Measured,
    /// The pessimistic profile of the old sender (finding 5915907311, #147),
    /// kept as a stress case: 10–33 ms; one block in 900 a single 40–80 ms
    /// late, one in 1 800 a clump of 2 (its first block a slot + 10–33 ms
    /// late, the next with it), one in 3 600 a clump of 3 (two slots +
    /// 0–10 ms). The ones past the ring's ~61.7 ms cushion underrun the card
    /// (the audio is not there yet); the excess each such underrun leaves is
    /// kept as cushion (Q1), so the next late block finds the ring fuller.
    Realistic,
    /// The same, but every late block within the cushion: singles 40–56 ms,
    /// clumps of 2 only (a slot + 0–20 ms).
    Absorbable,
}

#[derive(Clone, Copy)]
struct Case {
    rate: f64,
    buffer: u64,
    card_ppm: f64,
    jitter_s: f64,
    lateness: Lateness,
    step_s: f64,
    step_at_s: f64,
    drop_at_s: Option<f64>,
    /// The worker stalls from this boundary (s) for `stall_s`: those blocks
    /// are all handled at its end, back to back.
    stall_at_s: Option<f64>,
    stall_s: f64,
    /// From this second on the card calls back with this many frames.
    buffer_after: Option<(f64, u64)>,
}

const SNV: Case = Case {
    rate: 96_000.0,
    buffer: 128,
    card_ppm: 0.0,
    jitter_s: 0.015,
    lateness: Lateness::Uniform,
    step_s: 0.0,
    step_at_s: 400.0,
    drop_at_s: None,
    stall_at_s: None,
    stall_s: 0.0,
    buffer_after: None,
};
const RUN_S: f64 = 900.0;
const SLOT_S: f64 = SLOT_100NS as f64 / 1e7;

impl Lcg {
    /// One block's lateness (s), drawn as the case says.
    fn lateness(&mut self, c: &Case) -> f64 {
        match c.lateness {
            Lateness::Uniform => self.unit() * c.jitter_s,
            Lateness::Measured => {
                let band = self.unit();
                let v = self.unit();
                if band < 0.5 {
                    0.000_030 + v * 0.000_040
                } else if band < 0.99 {
                    0.000_070 + v * 0.000_071
                } else if band < 0.999 {
                    0.000_141 + v * 0.000_101
                } else {
                    0.000_242 + v * (0.009_36 - 0.000_242)
                }
            }
            Lateness::Realistic => {
                let normal = 0.010 + self.unit() * 0.023;
                let e = self.unit();
                if e < 1.0 / 900.0 {
                    0.040 + self.unit() * 0.040
                } else if e < 1.0 / 900.0 + 1.0 / 1800.0 {
                    SLOT_S + normal
                } else if e < 1.0 / 900.0 + 1.0 / 1800.0 + 1.0 / 3600.0 {
                    2.0 * SLOT_S + self.unit() * 0.010
                } else {
                    normal
                }
            }
            Lateness::Absorbable => {
                let normal = 0.010 + self.unit() * 0.023;
                let e = self.unit();
                if e < 1.0 / 900.0 {
                    0.040 + self.unit() * 0.016
                } else if e < 1.0 / 900.0 + 1.0 / 1800.0 {
                    SLOT_S + self.unit() * 0.020
                } else {
                    normal
                }
            }
        }
    }
}

#[derive(Debug, Default)]
struct Outcome {
    underruns: u64,
    max_abs_ppm: f64,
    final_ppm: f64,
    worst_slew_excess: f64,
    hard_recentres: u64,
    worst_latency_err_ms: f64,
    /// Re-centres asked while a skip was still running (a skip asked twice).
    asked_while_pending: u64,
    /// The last block after the step whose latency was more than 2 ms off
    /// the target, s after the step (`None`: none was).
    out_of_band_s: Option<f64>,
    /// The correction's largest distance from the card after [`SETTLED_S`]
    /// (a kick shows here; the final value alone hides it).
    max_off_card_ppm: f64,
    /// The same after [`STEADY_S`]: the steady state.
    max_off_card_steady_ppm: f64,
    /// The largest cushion the servo kept (ms).
    max_cushion_ms: f64,
    /// The cushion and the latency error (ms) at the last block.
    final_cushion_ms: f64,
    final_err_ms: f64,
}

/// The rate is locked (60 s) and the correction has slewed to it by then.
const SETTLED_S: f64 = 120.0;
/// Past the lock's early spread (the regression's span is still short).
const STEADY_S: f64 = 300.0;

/// The card: frames waiting, frames taken, the next callback (s), the one
/// callback to drop, and the underrun frames the worker reports (a whole
/// buffer per short callback, once the first block primed the ring).
struct Card {
    c: Case,
    fill: f64,
    consumed: u64,
    next_cb: f64,
    dropped: bool,
    primed: bool,
    underrun_frames: u64,
}

impl Card {
    /// Every callback due by `until` (s); counts an underrun after 3 s.
    fn run_until(&mut self, until: f64, out: &mut Outcome) {
        while self.next_cb <= until {
            let buffer = match self.c.buffer_after {
                Some((at, b)) if self.next_cb >= at => b,
                _ => self.c.buffer,
            };
            let period = buffer as f64 / (self.c.rate * (1.0 + self.c.card_ppm * 1e-6));
            let drop_now = self
                .c
                .drop_at_s
                .is_some_and(|t| !self.dropped && self.next_cb >= t);
            if drop_now {
                self.dropped = true; // the host missed it: nothing taken, nothing counted
            } else if self.fill >= buffer as f64 {
                self.fill -= buffer as f64;
                self.consumed += buffer;
            } else {
                if self.next_cb > 3.0 {
                    out.underruns += 1;
                }
                if self.primed {
                    self.underrun_frames += buffer;
                }
                self.fill = 0.0;
                self.consumed += buffer;
            }
            self.next_cb += period;
        }
    }
}

fn run(c: Case) -> Outcome {
    let mut servo = Servo::new(c.rate, BASE_LATENCY_100NS).with_callback_frames(c.buffer as u32);
    let mut rng = Lcg(7);
    let mut card = Card {
        c,
        fill: 0.0,
        consumed: 0,
        next_cb: 0.0,
        dropped: false,
        primed: false,
        underrun_frames: 0,
    };
    let mut out = Outcome::default();
    let (mut carry, mut last_handled, mut shift, mut stepped) = (0.0f64, 0.0f64, 0.0f64, false);
    let mut pending: u64 = 0;
    let mut last_change: Option<(i64, f64)> = None;
    let per_block = 1600.0 * c.rate / 48_000.0;
    // The splice's hold: buffered for the servo, not yet in the card's ring.
    let hold = crate::playback::asrc::Splice::new(c.rate, 0, 0).held_frames() as u64;
    // A 900 s run is 27 000 blocks; the bound only stops a broken loop.
    for k in 0..40_000i64 {
        let stamp_s = k as f64 * SLOT_S;
        if !stepped && stamp_s >= c.step_at_s {
            shift -= c.step_s;
            stepped = true;
        }
        let mut handled = (stamp_s + 0.5 + shift + rng.lateness(&c)).max(last_handled);
        if let Some(at) = c.stall_at_s
            && (at..at + c.stall_s).contains(&stamp_s)
        {
            handled = handled.max(at + c.stall_s + 0.5 + shift);
        }
        if handled > RUN_S {
            break;
        }
        card.run_until(handled, &mut out);
        last_handled = handled;
        let wall_s = handled - 0.5 - shift;
        let handled_100ns = (wall_s * 1e7).round() as i64;
        let a = servo.observe(Observation {
            handled_100ns,
            stamp_100ns: (stamp_s * 1e7).round() as i64,
            buffered_frames: card.fill.floor() as u64 + hold,
            pending_skip_frames: pending,
            consumed_frames: card.consumed,
            underrun_frames: card.underrun_frames,
        });
        card.primed = true;
        let latency_s = (card.fill + hold as f64 - pending as f64) / c.rate + (wall_s - stamp_s);
        let err_ms =
            (latency_s + a.recentre_100ns as f64 / 1e7 - BASE_LATENCY_100NS as f64 / 1e7) * 1e3;
        let quiet = (handled - (c.step_at_s + 0.5)).abs() > 3.0
            && c.drop_at_s.is_none_or(|t| (handled - t).abs() > 3.0)
            && c.stall_at_s.is_none_or(|t| (handled - t).abs() > 3.0);
        if handled > 70.0 && quiet {
            out.worst_latency_err_ms = out.worst_latency_err_ms.max(err_ms.abs());
        }
        if handled > c.step_at_s + 0.5 && err_ms.abs() > 2.0 {
            out.out_of_band_s = Some(handled - (c.step_at_s + 0.5));
        }
        out.max_abs_ppm = out.max_abs_ppm.max(a.correction_ppm.abs());
        if handled > SETTLED_S {
            let off = (a.correction_ppm - c.card_ppm).abs();
            out.max_off_card_ppm = out.max_off_card_ppm.max(off);
            if handled > STEADY_S {
                out.max_off_card_steady_ppm = out.max_off_card_steady_ppm.max(off);
            }
        }
        let cushion_ms = servo.status().cushion_ms;
        out.max_cushion_ms = out.max_cushion_ms.max(cushion_ms);
        out.final_cushion_ms = cushion_ms;
        out.final_err_ms = err_ms;
        if last_change.is_none_or(|(_, p)| p != a.correction_ppm) {
            if let Some((t, p)) = last_change {
                let allowed = MAX_SLEW_PPM_PER_S * ((handled_100ns - t) as f64 / 1e7);
                let moved = (a.correction_ppm - p).abs();
                out.worst_slew_excess = out.worst_slew_excess.max(moved - allowed);
            }
            last_change = Some((handled_100ns, a.correction_ppm));
        }
        out.final_ppm = a.correction_ppm;
        if a.recentre_100ns != 0 && pending > 0 {
            out.asked_while_pending += 1;
        }
        let frames = frames_from_100ns(a.recentre_100ns, c.rate);
        if frames > 0 {
            card.fill += frames as f64;
        } else {
            pending += frames.unsigned_abs();
        }
        let produced = per_block * (1.0 + a.correction_ppm * 1e-6) + carry;
        let whole = produced.floor();
        carry = produced - whole;
        let take = (pending as f64).min(whole);
        pending -= take as u64;
        card.fill += whole - take;
    }
    out.hard_recentres = servo.status().hard_recentres;
    out
}

/// How close a case must hold: the per-block latency error (ms) and the
/// final correction's distance from the card (ppm).
#[derive(Clone, Copy)]
struct Bounds {
    latency_ms: f64,
    final_ppm: f64,
}

const HELD: Bounds = Bounds {
    latency_ms: 2.0,
    final_ppm: 5.0,
};

/// No underrun, the ±300 ppm budget, the slew, no skip asked twice, and no
/// hard re-centre.
fn assert_smooth(o: &Outcome) {
    assert_eq!(o.underruns, 0, "{o:?}");
    assert_ratio_only(o);
}

/// The budget, the slew, no skip asked twice, no hard re-centre (underruns
/// allowed: a hand-off later than the cushion).
fn assert_ratio_only(o: &Outcome) {
    assert!(o.max_abs_ppm <= MAX_PPM, "{o:?}");
    assert!(o.worst_slew_excess <= 1e-9, "{o:?}");
    assert_eq!(o.asked_while_pending, 0, "{o:?}");
    assert_eq!(o.hard_recentres, 0, "{o:?}");
}

fn assert_within(o: &Outcome, card_ppm: f64, b: Bounds) {
    assert_smooth(o);
    assert!(o.worst_latency_err_ms <= b.latency_ms, "{o:?}");
    assert!((o.final_ppm - card_ppm).abs() <= b.final_ppm, "{o:?}");
}

#[test]
fn a_card_at_minus_50_0_and_plus_50_ppm_is_followed() {
    for ppm in [-50.0, 0.0, 50.0] {
        let o = run(Case {
            card_ppm: ppm,
            ..SNV
        });
        assert_within(&o, ppm, HELD);
    }
}

/// ±1 ms: inside the calm zone, the level loop takes it (three window
/// phases: a pin at one phase can be phase-lucky).
#[test]
fn a_1_ms_clock_step_either_way_is_absorbed_by_the_ratio() {
    for step in [0.001, -0.001] {
        for phase in [0.0, 0.35, 0.7] {
            let c = Case {
                card_ppm: 20.0,
                step_s: step,
                step_at_s: 400.0 + phase,
                ..SNV
            };
            assert_within(&run(c), 20.0, HELD);
        }
    }
}

/// A whole slot forward, a date step's remainder (the fleet relabels whole
/// slots and moves the timeline forward by the rest, `genlock.md`). On a 0
/// and a 20 ppm card at three window phases: slewed, never spliced, and
/// back within ±2 ms of the target in at most 150 s (the model:
/// 138.9–145.4 s).
#[test]
fn a_33_ms_forward_step_is_slewed_back_within_2_ms_in_150_s() {
    for card_ppm in [0.0, 20.0] {
        for phase in [0.0, 0.35, 0.7] {
            let o = run(Case {
                card_ppm,
                step_s: 0.033,
                step_at_s: 400.0 + phase,
                ..SNV
            });
            assert_smooth(&o);
            let back = o.out_of_band_s.expect("the step moved the latency");
            assert!(back <= 150.0, "{back} s: {o:?}");
            assert!((o.final_ppm - card_ppm).abs() <= 5.0, "{o:?}");
        }
    }
}

/// A missing boundary (−33 ms, a whole slot back): the latency falls under
/// the floor of the splice's hold + one slot (Q2), so it is ONE faded insert
/// at once — a hard re-centre, counted — instead of a run of underruns
/// while the slew restores the ring. On a 0 and a 20 ppm card at three
/// window phases: no underrun, no block more than 2 ms off after it, no
/// skip asked twice.
#[test]
fn a_missing_boundary_is_one_faded_insert() {
    for card_ppm in [0.0, 20.0] {
        for phase in [0.0, 0.35, 0.7] {
            let o = run(Case {
                card_ppm,
                step_s: -0.033,
                step_at_s: 400.0 + phase,
                ..SNV
            });
            assert_eq!(
                (o.hard_recentres, o.underruns, o.asked_while_pending),
                (1, 0, 0),
                "{o:?}"
            );
            assert_eq!(o.out_of_band_s, None, "{o:?}");
            assert!(o.max_abs_ppm <= MAX_PPM, "{o:?}");
            assert!(o.worst_slew_excess <= 1e-9, "{o:?}");
            assert!((o.final_ppm - card_ppm).abs() <= 5.0, "{o:?}");
        }
    }
}

/// The nominal case: SP-program's hand-off as measured (sub-millisecond,
/// [`Lateness::Measured`]) at 128 and 512 frames, 96 kHz, on a 0 and a
/// 20 ppm card: no underrun, no hard re-centre, the latency within the
/// callback sawtooth and the correction within a few ppm of the card in the
/// steady state (the model: 1.52 / 1.34 / 4.67 / 5.10 ms, 2.79 / 1.32 /
/// 3.00 / 3.35 ppm).
#[test]
fn the_measured_hand_off_runs_with_no_underrun_and_no_hard_re_centre() {
    for (buffer, card_ppm, latency_ms, steady_ppm) in [
        (128, 0.0, 1.6, 2.9),
        (128, 20.0, 1.4, 1.4),
        (512, 0.0, 4.7, 3.1),
        (512, 20.0, 5.2, 3.4),
    ] {
        let o = run(Case {
            buffer,
            card_ppm,
            lateness: Lateness::Measured,
            ..SNV
        });
        assert_smooth(&o);
        assert!(o.worst_latency_err_ms <= latency_ms, "{o:?}");
        assert!(o.max_off_card_steady_ppm <= steady_ppm, "{o:?}");
        assert!((o.final_ppm - card_ppm).abs() <= 2.5, "{o:?}");
    }
}

/// Jitter never moves the output: every late or clumped block within the
/// ring's cushion leaves the latency and the ratio where they were — no
/// underrun, the latency within half a callback period of the sawtooth, the
/// correction never more than 1.3 / 2.5 / 1.8 ppm off the card once settled
/// (no kick; the model: 1.28, 2.47, 1.74).
#[test]
fn a_hand_off_late_within_the_cushion_moves_nothing() {
    for (rate, buffer, card_ppm, latency_ms, off_card_ppm) in [
        (96_000.0, 128, 0.0, 1.4, 1.3),
        (96_000.0, 512, 0.0, 3.9, 2.5),
        (96_000.0, 128, -50.0, 1.7, 1.8),
    ] {
        let o = run(Case {
            rate,
            buffer,
            card_ppm,
            lateness: Lateness::Absorbable,
            ..SNV
        });
        assert_smooth(&o);
        assert!(o.worst_latency_err_ms <= latency_ms, "{o:?}");
        assert!(o.max_off_card_ppm <= off_card_ppm, "{o:?}");
        assert!((o.final_ppm - card_ppm).abs() <= 2.5, "{o:?}");
    }
}

/// The pessimistic hand-off (the stress case): blocks past the cushion
/// underrun the card, and the excess each leaves is KEPT as cushion (Q1, at
/// most one slot), so the next late block finds the ring fuller — never a
/// hard re-centre. The model's underrun callbacks: 29, 13, 13, 30 (97, 33,
/// 33, 99 while the stop curve drained each excess), and the correction
/// never more than ~37 ppm off the card after 120 s (over 50 ppm ~75 % of
/// that time while it drained).
#[test]
fn a_realistic_hand_off_with_clumps_needs_no_hard_re_centre() {
    for (rate, buffer, card_ppm, underruns) in [
        (96_000.0, 128, 0.0, 29),
        (96_000.0, 512, 0.0, 13),
        (48_000.0, 256, 0.0, 13),
        (96_000.0, 128, 50.0, 30),
    ] {
        let o = run(Case {
            rate,
            buffer,
            card_ppm,
            lateness: Lateness::Realistic,
            ..SNV
        });
        assert_ratio_only(&o);
        assert_eq!(o.underruns, underruns, "{o:?}");
        assert!(o.worst_latency_err_ms <= 22.0, "{o:?}");
        assert!(o.max_off_card_ppm <= 38.0, "{o:?}");
        assert!(o.max_cushion_ms > 0.0, "{o:?}");
    }
}

/// The card's callback period goes from 128 to 512 frames mid-run with no
/// reopen (its sawtooth moves the mean reading by 2 ms): slewed, no
/// underrun. A real driver's buffer-size change is a reset (the worker
/// reopens and primes: `asio_state::close_reason`), never this path.
#[test]
fn a_callback_period_that_changes_mid_run_is_slewed() {
    let o = run(Case {
        buffer_after: Some((400.0, 512)),
        ..SNV
    });
    assert_smooth(&o);
    assert!(o.worst_latency_err_ms <= 4.5, "{o:?}");
}

/// The program catches up 100 ms at once: under the last resort's four
/// slots, so the ratio drains it (in about 6 min) — no splice.
#[test]
fn a_100_ms_forward_step_is_slewed_without_a_re_centre() {
    let o = run(Case {
        card_ppm: 20.0,
        step_s: 0.100,
        ..SNV
    });
    assert_smooth(&o);
    let back = o.out_of_band_s.expect("the step moved the latency");
    assert!(back <= 360.0, "{back} s: {o:?}");
    assert!((o.final_ppm - 20.0).abs() <= 5.0, "{o:?}");
}

#[test]
fn a_dropped_buffer_is_absorbed() {
    let c = Case {
        card_ppm: -20.0,
        drop_at_s: Some(500.0),
        ..SNV
    };
    assert_within(
        &run(c),
        -20.0,
        Bounds {
            latency_ms: 3.0,
            ..HELD
        },
    );
}

/// A 1024-frame driver at 48 kHz (a 21.3 ms callback period, a 10.7 ms calm
/// zone): a 20 ms step is slewed; the per-block reading carries the
/// sawtooth itself.
#[test]
fn a_1024_frame_driver_at_48k_slews_a_20_ms_step() {
    let o = run(Case {
        rate: 48_000.0,
        buffer: 1024,
        card_ppm: 20.0,
        step_s: 0.020,
        ..SNV
    });
    assert_smooth(&o);
    assert!(o.worst_latency_err_ms <= 32.0, "{o:?}");
}

#[test]
fn a_48k_card_with_256_frame_buffers_is_followed() {
    let c = Case {
        rate: 48_000.0,
        buffer: 256,
        card_ppm: 50.0,
        ..SNV
    };
    assert_within(
        &run(c),
        50.0,
        Bounds {
            latency_ms: 4.5,
            ..HELD
        },
    );
}

/// The worker stalls 150 ms: the card runs dry (underruns: the audio is
/// not there), the late blocks come back to back, and the excess they leave
/// (the stall less the ring's cushion) is under the last resort's four
/// slots — never a splice, at four phases of the slot (review round 2: a
/// single phase can be lucky). One slot of it is kept as cushion (Q1; the
/// rest is slewed away), and it decays slowly through the level loop: 400 s
/// later ~19–22 ms are left (the model: 18.9–22.0 ms of cushion), the
/// correction still under the card by P + I (≤ 53 ppm).
#[test]
fn a_150_ms_worker_stall_is_slewed_at_every_phase() {
    for phase in [0.0, 0.0083, 0.0167, 0.025] {
        let o = run(Case {
            card_ppm: 20.0,
            stall_at_s: Some(500.0 + phase),
            stall_s: 0.150,
            ..SNV
        });
        assert!(o.underruns > 0, "the ring ran dry: {o:?}");
        assert_ratio_only(&o);
        assert_eq!(o.max_cushion_ms, 33.3333, "one slot kept: {o:?}");
        assert!(
            (18.0..=23.0).contains(&o.final_cushion_ms),
            "decaying: {o:?}"
        );
        assert!(
            (0.0..=o.final_cushion_ms + 1.0).contains(&o.final_err_ms),
            "{o:?}"
        );
        assert!((-53.0..=0.0).contains(&(o.final_ppm - 20.0)), "{o:?}");
    }
}

/// The ring-limit cases: a 300 ms stall leaves more than four slots over (the
/// ring would overflow; four phases of the slot), a 100 ms pause a latency
/// under the floor (38.3 ms): exactly one hard re-centre each, the skip
/// asked once; the cushion its underruns left goes with the re-centre.
#[test]
fn a_300_ms_stall_or_a_100_ms_pause_forces_one_hard_re_centre() {
    let stall = |phase: f64| Case {
        card_ppm: 20.0,
        stall_at_s: Some(500.0 + phase),
        stall_s: 0.300,
        ..SNV
    };
    let pause = Case {
        card_ppm: 20.0,
        step_s: -0.100,
        ..SNV
    };
    for c in [
        stall(0.0),
        stall(0.0083),
        stall(0.0167),
        stall(0.025),
        pause,
    ] {
        let o = run(c);
        assert_eq!((o.hard_recentres, o.asked_while_pending), (1, 0), "{o:?}");
        assert!(o.max_abs_ppm <= MAX_PPM, "{o:?}");
        assert!(o.worst_slew_excess <= 1e-9, "{o:?}");
        assert!((o.final_ppm - 20.0).abs() <= 5.0, "{o:?}");
        assert_eq!(o.final_cushion_ms, 0.0, "{o:?}");
    }
}
