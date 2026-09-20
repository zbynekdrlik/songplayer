//! #192 round 4: video follows the wall-clock audio after a producer stall.
//!
//! Round 3's 1.5 s cushion stops a ~1 s producer stall (a resident stems child)
//! from reaching the speakers, but it exposed a follow-on defect: the audio
//! emitter keeps its wall-clock grid while the video, submitted at DECODE time
//! on the SDK-clocked path, resumes ~1.4 s late and never catches up (the SDK
//! clock only throttles EARLY frames, never accelerates LATE ones), so the lips
//! stay ~1.4 s behind for the rest of the song.
//!
//! The decoder reads audio ahead of the video by
//! `DEFAULT_TOLERANCE_MS + AUDIO_LOOKAHEAD_MS` (= 1540 ms), so the ring depth IS
//! the video's lag measurement: `lag_ms = target_depth_ms − ring_depth_ms`. This
//! pure module decides, per decoded frame, whether to SUBMIT it or DROP its video
//! (its audio is already queued in the ring) to let the video catch up at decode
//! speed (~2.6× real time) — a ~0.5 s fast-forward instead of a lasting offset.
//!
//! Pure and cross-platform (Linux-tested, mutation-scored). The single call site
//! in the Windows-only decode loop lives in `pipeline.rs`; the ring-lock glue
//! that feeds this module the live depth lives in `pipeline_audio.rs`.

/// Max consecutive video-frame drops before ONE frame is submitted anyway, so a
/// decoder that genuinely cannot catch up never leaves the wall black. ≈ 3 s of
/// frames at `DEFAULT_TOLERANCE_MS` (40 ms) per frame (3000 / 40 = 75).
pub const MAX_CONSECUTIVE_DROPS: u32 = 75;

/// Whether to submit this decoded video frame to NDI, or drop its video to let
/// the video timeline catch up to the wall-clock audio position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Submit the video frame (on time, or caught up, or the drop cap forced it).
    Submit,
    /// Drop this frame's video — its audio is already in the emitter ring.
    Drop,
}

/// The pure per-frame rule: DROP only once the ring has been primed in this song
/// AND the video lags the emitted audio by MORE than one frame.
///
/// `target_depth_ms` is the nominal ring depth
/// (`DEFAULT_TOLERANCE_MS + AUDIO_LOOKAHEAD_MS`); the video lag IS
/// `target_depth_ms − ring_depth_ms`. Never drops below the target − one frame prime
/// threshold (so the whole initial fill / post-seek refill up to 90 % is safe —
/// see [`CatchUp::note_depth`]) and never once caught up (lag within one frame);
/// the top ~10 % of a fill that is still lagging may drop a few frames, which the
/// SDK clock re-times harmlessly. `saturating_sub` so a ring deeper than target
/// (video ahead) reads zero lag, never an underflow.
pub fn decide(ring_depth_ms: u64, target_depth_ms: u64, frame_ms: u64, primed: bool) -> Decision {
    let lag_ms = target_depth_ms.saturating_sub(ring_depth_ms);
    if primed && lag_ms > frame_ms {
        Decision::Drop
    } else {
        Decision::Submit
    }
}

/// Per-song catch-up state for the SDK-clocked decode loop: the prime latch (so
/// the fill below target − one frame is never dropped as a stall) and the
/// consecutive-drop cap. `Default` is the fresh, un-primed state a new song
/// starts in.
#[derive(Clone, Debug, Default)]
pub struct CatchUp {
    primed: bool,
    consecutive_drops: u32,
}

impl CatchUp {
    /// A fresh, un-primed catch-up state for a new song.
    pub fn new() -> Self {
        Self::default()
    }

    /// New play / seek (a `clear_ring` site): forget the prime latch and the
    /// drop run, so the sub-target − one frame portion of the post-seek refill re-primes
    /// from scratch and is never dropped as a stall.
    pub fn reset(&mut self) {
        self.primed = false;
        self.consecutive_drops = 0;
    }

    /// Whether the ring has reached the prime threshold in this song.
    pub fn primed(&self) -> bool {
        self.primed
    }

    /// Latch `primed` the first time the ring is within ONE FRAME of the target
    /// in this song (`depth + frame ≥ target`) — i.e. exactly when [`decide`]
    /// would already say `Submit` for that depth, so the priming frame (and the
    /// whole song-start fill, during which the video is on time and the depth is
    /// not a lag) is never dropped. Once latched it stays until
    /// [`reset`](Self::reset); a stall that drains the ring never un-primes it.
    fn note_depth(&mut self, ring_depth_ms: u64, target_depth_ms: u64, frame_ms: u64) {
        if !self.primed && ring_depth_ms.saturating_add(frame_ms) >= target_depth_ms {
            self.primed = true;
        }
    }

    /// The stateful per-frame decision: latch priming from the current depth,
    /// apply [`decide`], and enforce [`MAX_CONSECUTIVE_DROPS`] — after a full run
    /// of drops, submit one frame anyway and restart the run so a stuck decoder
    /// never blacks the wall.
    pub fn step(&mut self, ring_depth_ms: u64, target_depth_ms: u64, frame_ms: u64) -> Decision {
        self.note_depth(ring_depth_ms, target_depth_ms, frame_ms);
        match decide(ring_depth_ms, target_depth_ms, frame_ms, self.primed) {
            Decision::Drop => {
                if self.consecutive_drops > MAX_CONSECUTIVE_DROPS {
                    self.consecutive_drops = 0;
                    Decision::Submit
                } else {
                    self.consecutive_drops += 1;
                    Decision::Drop
                }
            }
            Decision::Submit => {
                self.consecutive_drops = 0;
                Decision::Submit
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The production target depth (DEFAULT_TOLERANCE_MS 40 + AUDIO_LOOKAHEAD_MS
    // 1500) and the one-frame budget (the sync decoder's pairing tolerance).
    const TARGET: u64 = 1540;
    const FRAME: u64 = 40;

    #[test]
    fn decide_never_drops_before_primed() {
        // A huge lag (empty ring) must still SUBMIT while un-primed — the initial
        // fill at song start must never be mistaken for a stall.
        assert_eq!(decide(0, TARGET, FRAME, false), Decision::Submit);
        assert_eq!(decide(TARGET / 2, TARGET, FRAME, false), Decision::Submit);
    }

    #[test]
    fn decide_lag_exactly_one_frame_submits() {
        // lag == frame is NOT "more than one frame behind" → Submit (kills >= / <=).
        let depth = TARGET - FRAME; // lag == FRAME exactly
        assert_eq!(decide(depth, TARGET, FRAME, true), Decision::Submit);
    }

    #[test]
    fn decide_lag_one_ms_over_frame_drops() {
        // lag == frame + 1 → the first value that is "more than one frame" → Drop.
        let depth = TARGET - FRAME - 1;
        assert_eq!(decide(depth, TARGET, FRAME, true), Decision::Drop);
    }

    #[test]
    fn decide_caught_up_submits_even_when_primed() {
        // Ring at target (lag 0) and ring DEEPER than target (video ahead) both
        // Submit — kills a `!=` mutant on the lag comparison and a `saturating_sub
        // → saturating_add` mutant.
        assert_eq!(decide(TARGET, TARGET, FRAME, true), Decision::Submit);
        assert_eq!(decide(TARGET + 500, TARGET, FRAME, true), Decision::Submit);
    }

    #[test]
    fn prime_latches_at_exactly_target_minus_one_frame() {
        // depth + frame == target (1500 + 40 == 1540) → primes, and the priming
        // frame itself is submitted (lag == one frame).
        let mut c = CatchUp::new();
        assert!(!c.primed());
        assert_eq!(c.step(1500, TARGET, FRAME), Decision::Submit);
        assert!(c.primed(), "target − one frame exactly must latch primed");
    }

    #[test]
    fn prime_does_not_latch_just_below_target_minus_one_frame() {
        // 1499 + 40 = 1539 < 1540 → must NOT prime; a frame this shallow is the
        // initial fill, not yet a stall — so it Submits (never Drops).
        let mut c = CatchUp::new();
        assert_eq!(c.step(1499, TARGET, FRAME), Decision::Submit);
        assert!(!c.primed(), "just below target − one frame must not latch");
    }

    #[test]
    fn prime_latches_well_above_threshold() {
        // A full ring primes (kills a `>=` → `==` mutant on note_depth, which
        // would only match at the exact boundary).
        let mut c = CatchUp::new();
        c.step(TARGET, TARGET, FRAME);
        assert!(c.primed());
    }

    #[test]
    fn the_frame_that_primes_the_latch_is_never_dropped() {
        // Song-start fill: the ring grows 0 → target while the video is ON TIME
        // (the SDK clock paces early frames), so the depth is NOT a lag yet.
        // Priming must happen only once the ring is within one frame of the
        // target, so the priming frame itself — and every fill frame before
        // it — is submitted. (A target − one frame latch dropped ~4 on-time frames at
        // every song start; the SDK paces by frame COUNT, so those drops moved
        // the video AHEAD of the audio for the rest of the song.)
        let mut c = CatchUp::new();
        for depth in (0..=1500).step_by(20) {
            assert_eq!(
                c.step(depth, TARGET, FRAME),
                Decision::Submit,
                "fill depth {depth} must submit"
            );
        }
        assert!(c.primed(), "within one frame of target must be primed");
        // Now a real stall: the ring drains → drops until caught up.
        assert_eq!(c.step(700, TARGET, FRAME), Decision::Drop);
        assert_eq!(c.step(1500, TARGET, FRAME), Decision::Submit);
    }

    #[test]
    fn prime_persists_after_the_ring_drains() {
        // Once primed by a full ring, a later stall (shallow ring) keeps primed
        // AND now Drops — the exact round-4 behaviour.
        let mut c = CatchUp::new();
        assert_eq!(c.step(TARGET, TARGET, FRAME), Decision::Submit); // primes
        assert_eq!(c.step(150, TARGET, FRAME), Decision::Drop); // stall → drop
        assert!(c.primed());
    }

    #[test]
    fn step_submit_resets_the_drop_run() {
        // A run of drops, then one caught-up Submit, must reset the consecutive
        // count so a LATER stall gets the full cap again (kills the Submit-arm
        // `consecutive_drops = 0` reset).
        let mut c = CatchUp::new();
        c.step(TARGET, TARGET, FRAME); // prime
        for _ in 0..50 {
            assert_eq!(c.step(150, TARGET, FRAME), Decision::Drop);
        }
        assert_eq!(c.step(TARGET, TARGET, FRAME), Decision::Submit); // caught up → reset
        // A fresh stall now takes the full run again, not (cap − 50).
        for _ in 0..(MAX_CONSECUTIVE_DROPS + 1) {
            assert_eq!(c.step(150, TARGET, FRAME), Decision::Drop);
        }
        assert_eq!(c.step(150, TARGET, FRAME), Decision::Submit); // forced by the cap
    }

    #[test]
    fn drop_cap_forces_a_submit_after_max_consecutive_drops() {
        // After MAX_CONSECUTIVE_DROPS consecutive drops the next frame is submitted
        // anyway (so a decoder that cannot catch up never blacks the wall), and the
        // run restarts. Exact boundary pins the `>` cap comparison.
        let mut c = CatchUp::new();
        c.step(TARGET, TARGET, FRAME); // prime
        for i in 0..=MAX_CONSECUTIVE_DROPS {
            assert_eq!(
                c.step(150, TARGET, FRAME),
                Decision::Drop,
                "drop {i} of the run (up to and including MAX) must Drop"
            );
        }
        // One more consecutive late frame → the cap forces a Submit.
        assert_eq!(c.step(150, TARGET, FRAME), Decision::Submit);
        // The run restarted: the very next late frame Drops again.
        assert_eq!(c.step(150, TARGET, FRAME), Decision::Drop);
    }

    #[test]
    fn reset_clears_prime_and_the_drop_run() {
        // Seek / new play forgets everything: an un-primed shallow ring Submits.
        let mut c = CatchUp::new();
        c.step(TARGET, TARGET, FRAME); // prime
        c.step(150, TARGET, FRAME); // one drop
        c.reset();
        assert!(!c.primed());
        assert_eq!(
            c.step(150, TARGET, FRAME),
            Decision::Submit,
            "post-reset refill Submits"
        );
    }

    #[test]
    fn reset_restarts_the_drop_run() {
        // reset() must clear the consecutive-drop count, not just the prime latch:
        // a seek that lands where the ring is already at target − one frame (primes AND
        // lags on the very first frame) must still get the FULL cap, not a short
        // one carried over from before the seek.
        let mut c = CatchUp::new();
        c.step(TARGET, TARGET, FRAME); // prime
        for _ in 0..70 {
            c.step(150, TARGET, FRAME); // build the run up to 70 drops
        }
        c.reset();
        // depth 1386 = target − one frame: primes on this first post-reset frame AND lags
        // (1540 − 1386 = 154 > one frame), so the run restarts from zero here.
        for i in 0..=MAX_CONSECUTIVE_DROPS {
            assert_eq!(
                c.step(1386, TARGET, FRAME),
                Decision::Drop,
                "post-reset drop {i} must Drop — the pre-seek run must not carry over"
            );
        }
        assert_eq!(c.step(1386, TARGET, FRAME), Decision::Submit);
    }

    #[test]
    fn max_consecutive_drops_is_about_three_seconds() {
        // ≈ 3 s of 40 ms frames — a pinned invariant so the safety valve stays a
        // few seconds, not a tunable that silently drifts.
        assert_eq!(MAX_CONSECUTIVE_DROPS, 75);
    }
}
