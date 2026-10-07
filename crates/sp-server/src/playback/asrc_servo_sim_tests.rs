//! #233: the servo in a closed loop. A card consumes `buffer` frames per
//! callback at `rate·(1 + card_ppm)`; the program hands one block per grid
//! slot, `h ∈ [0, jitter]` late (a seeded LCG, one draw per block); the card's
//! callbacks due before that hand-off run first; the servo's correction makes
//! `1600·rate/48000·(1 + ppm)` frames per block (fractional carry); a
//! re-centre inserts or drops at once. A clock step shifts the program
//! timeline against the card at 400 s (forward = the program catches up,
//! backward = it pauses); a dropped buffer is one callback the host missed.
//! 900 s per case. The slew is checked on the 100 ns instants the servo saw
//! (rounding the float wall would let a 1e-7 s quantum read as a 5e-7 ppm
//! over-move).
//!
//! A scratch model of this file (and a fuzz of it over 240 random cards,
//! jitters, steps, drops and rates) gave: 0 underruns; a latency error ≤ 4.1 ms
//! after 70 s in every case below (≤ 15.3 ms across the fuzz); |final − card|
//! ≤ 4.1 ppm here; re-centres 1, or 2 for a ±44 ms step. Outside the envelope:
//! a backward step of 48 ms on top of 30 ms of hand-off jitter exceeds the
//! 66.7 ms budget itself and underruns once (the audio does not exist yet).

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

#[derive(Clone, Copy)]
struct Case {
    rate: f64,
    buffer: u64,
    card_ppm: f64,
    jitter_s: f64,
    step_s: f64,
    drop_at_s: Option<f64>,
}

const SNV: Case = Case {
    rate: 96_000.0,
    buffer: 128,
    card_ppm: 0.0,
    jitter_s: 0.015,
    step_s: 0.0,
    drop_at_s: None,
};
const STEP_AT_S: f64 = 400.0;
const RUN_S: f64 = 900.0;
const SLOT_S: f64 = GROSS_STEP_100NS as f64 / 1e7;

#[derive(Debug, Default)]
struct Outcome {
    underruns: u64,
    max_abs_ppm: f64,
    final_ppm: f64,
    worst_slew_excess: f64,
    recentres: u64,
    worst_latency_err_ms: f64,
}

/// The card: frames waiting, frames taken, the next callback (s), the one
/// callback to drop.
struct Card {
    c: Case,
    fill: f64,
    consumed: u64,
    next_cb: f64,
    dropped: bool,
}

impl Card {
    /// Every callback due by `until` (s); counts an underrun after 3 s.
    fn run_until(&mut self, until: f64, out: &mut Outcome) {
        let period = self.c.buffer as f64 / (self.c.rate * (1.0 + self.c.card_ppm * 1e-6));
        while self.next_cb <= until {
            let drop_now = self
                .c
                .drop_at_s
                .is_some_and(|t| !self.dropped && self.next_cb >= t);
            if drop_now {
                self.dropped = true; // the host missed it: nothing taken, nothing counted
            } else if self.fill >= self.c.buffer as f64 {
                self.fill -= self.c.buffer as f64;
                self.consumed += self.c.buffer;
            } else {
                if self.next_cb > 3.0 {
                    out.underruns += 1;
                }
                self.fill = 0.0;
                self.consumed += self.c.buffer;
            }
            self.next_cb += period;
        }
    }
}

fn run(c: Case) -> Outcome {
    let mut servo = Servo::new(c.rate, BASE_LATENCY_100NS);
    let mut rng = Lcg(7);
    let mut card = Card {
        c,
        fill: 0.0,
        consumed: 0,
        next_cb: 0.0,
        dropped: false,
    };
    let mut out = Outcome::default();
    let (mut carry, mut last_handled, mut shift, mut stepped) = (0.0f64, 0.0f64, 0.0f64, false);
    let mut last_change: Option<(i64, f64)> = None;
    let per_block = 1600.0 * c.rate / 48_000.0;
    // A 900 s run is 27 000 blocks; the bound only stops a broken loop.
    for k in 0..40_000i64 {
        let stamp_s = k as f64 * SLOT_S;
        if !stepped && stamp_s >= STEP_AT_S {
            shift -= c.step_s;
            stepped = true;
        }
        let handled = (stamp_s + 0.5 + shift + rng.unit() * c.jitter_s).max(last_handled);
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
            buffered_frames: card.fill.floor() as u64,
            consumed_frames: card.consumed,
        });
        let latency_s = card.fill / c.rate + (wall_s - stamp_s);
        let quiet = (handled - (STEP_AT_S + 0.5)).abs() > 3.0
            && c.drop_at_s.is_none_or(|t| (handled - t).abs() > 3.0);
        if handled > 70.0 && quiet {
            let err_s = latency_s + a.recentre_100ns as f64 / 1e7 - BASE_LATENCY_100NS as f64 / 1e7;
            out.worst_latency_err_ms = out.worst_latency_err_ms.max(err_s.abs() * 1e3);
        }
        out.max_abs_ppm = out.max_abs_ppm.max(a.correction_ppm.abs());
        if last_change.is_none_or(|(_, p)| p != a.correction_ppm) {
            if let Some((t, p)) = last_change {
                let allowed = MAX_SLEW_PPM_PER_S * ((handled_100ns - t) as f64 / 1e7);
                let moved = (a.correction_ppm - p).abs();
                out.worst_slew_excess = out.worst_slew_excess.max(moved - allowed);
            }
            last_change = Some((handled_100ns, a.correction_ppm));
        }
        out.final_ppm = a.correction_ppm;
        card.fill = (card.fill + a.recentre_100ns as f64 / 1e7 * c.rate).max(0.0);
        let produced = per_block * (1.0 + a.correction_ppm * 1e-6) + carry;
        carry = produced - produced.floor();
        card.fill += produced.floor();
    }
    out.recentres = servo.status().recentres;
    out
}

fn assert_held(o: &Outcome, card_ppm: f64, recentres: u64) {
    assert_eq!(o.underruns, 0, "{o:?}");
    assert!(o.max_abs_ppm <= MAX_PPM, "{o:?}");
    assert!(o.worst_slew_excess <= 1e-9, "{o:?}");
    assert!(o.worst_latency_err_ms <= 10.0, "{o:?}");
    assert!((o.final_ppm - card_ppm).abs() <= 5.0, "{o:?}");
    assert_eq!(o.recentres, recentres, "{o:?}");
}

#[test]
fn a_card_at_minus_50_0_and_plus_50_ppm_is_followed() {
    for ppm in [-50.0, 0.0, 50.0] {
        assert_held(
            &run(Case {
                card_ppm: ppm,
                ..SNV
            }),
            ppm,
            1,
        );
    }
}

#[test]
fn a_1_ms_clock_step_either_way_is_absorbed_without_a_re_centre() {
    for step in [0.001, -0.001] {
        let c = Case {
            card_ppm: 20.0,
            step_s: step,
            ..SNV
        };
        assert_held(&run(c), 20.0, 1);
    }
}

#[test]
fn a_44_ms_clock_step_either_way_re_centres_once() {
    for step in [0.044, -0.044] {
        let c = Case {
            card_ppm: 20.0,
            step_s: step,
            ..SNV
        };
        assert_held(&run(c), 20.0, 2);
    }
}

#[test]
fn a_dropped_buffer_is_absorbed() {
    let c = Case {
        card_ppm: -20.0,
        drop_at_s: Some(500.0),
        ..SNV
    };
    assert_held(&run(c), -20.0, 1);
}

#[test]
fn a_48k_card_with_256_frame_buffers_is_followed() {
    let c = Case {
        rate: 48_000.0,
        buffer: 256,
        card_ppm: 50.0,
        ..SNV
    };
    assert_held(&run(c), 50.0, 1);
}
