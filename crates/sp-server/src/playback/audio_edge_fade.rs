//! Pure per-block edge fades for the wall-clock audio emitter (#192 round 5).
//!
//! A seek, a stall, or a natural song end leaves the emitter ring empty right
//! after audio; the old emitter hard-CUT to silence (a click) and the first
//! audio block after a silence run started at FULL gain (a click). [`EdgeFade`]
//! shapes those two edges with a linear ramp, applied to the interleaved samples
//! the emit thread sends:
//!
//! - **fade-out tail** — the FIRST empty slot right after audio emits the last
//!   audio block ramped `1 → 0` (a 33 ms fade-out from real content) instead of a
//!   hard cut; every later empty slot is plain zero silence.
//! - **fade-in** — the first [`FADE_IN_BLOCKS`] audio blocks after a silence run
//!   ramp `0 → 1`, continuous across the blocks (100 ms).
//!
//! It is PURE and single-threaded — the emitter calls [`shape_audio`](EdgeFade::shape_audio)
//! for every audio block and [`shape_silence`](EdgeFade::shape_silence) for every
//! silence block, in slot order, inside `AudioEmitter::samples_for`. The ring, the
//! grid timecodes, the `silence_blocks` accounting and the `Emitted.block` variant
//! (so the transition log stays correct) are all UNTOUCHED — the fade only reshapes
//! the interleaved samples handed to NDI.

/// Audio blocks over which the post-silence fade-IN ramps `0 → 1` (3 × 33.3 ms ≈
/// 100 ms, continuous across the three blocks).
pub const FADE_IN_BLOCKS: u32 = 1;

/// The per-slot edge-fade state machine. Pure and single-threaded.
#[derive(Debug)]
pub struct EdgeFade {
    /// Frames (samples per channel) in one grid block — the fade-in ramp span is
    /// `FADE_IN_BLOCKS · samples_per_block` frames, the tail is one block.
    samples_per_block: usize,
    /// Any audio block emitted yet? Gates the tail so a silence-only start stays
    /// pure silence (no tail from a block that never played).
    seen_audio: bool,
    /// In a silence run — the next audio block restarts the fade-in.
    in_silence: bool,
    /// Audio blocks into the current post-silence run; `< FADE_IN_BLOCKS` while the
    /// fade-in ramp is active, then clamped there (full gain).
    fade_in_pos: u32,
    /// A verbatim copy of the last audio block emitted (pre-fade), reused as the
    /// fade-out tail source. Empty until audio has flowed.
    last_block: Vec<f32>,
    /// The previous slot emitted audio → the next empty slot is the single
    /// fade-out tail, not a hard cut.
    tail_pending: bool,
}

impl EdgeFade {
    pub fn new(samples_per_block: usize) -> Self {
        Self {
            samples_per_block,
            seen_audio: false,
            in_silence: false,
            fade_in_pos: 0,
            last_block: Vec::new(),
            tail_pending: false,
        }
    }

    /// Shape one AUDIO block the emitter is about to send: a fade-in ramp for the
    /// first [`FADE_IN_BLOCKS`] blocks after a silence run, else full gain. Saves
    /// the block (pre-fade) as the fade-out source and arms the tail.
    pub fn shape_audio(&mut self, samples: &[f32], channels: usize) -> Vec<f32> {
        self.seen_audio = true;
        if self.in_silence {
            // Silence → audio edge: restart the fade-in from block 0.
            self.in_silence = false;
            self.fade_in_pos = 0;
        }
        // Save a verbatim copy (reused buffer) for a possible fade-out tail.
        self.last_block.clear();
        self.last_block.extend_from_slice(samples);
        self.tail_pending = true;

        let fading = channels > 0 && self.fade_in_pos < FADE_IN_BLOCKS;
        let out = if fading {
            let base = self.fade_in_pos as usize * self.samples_per_block;
            let total = FADE_IN_BLOCKS as usize * self.samples_per_block;
            let frames = samples.len() / channels;
            let mut out = Vec::with_capacity(samples.len());
            for f in 0..frames {
                let gain = fade_in_gain(base + f, total);
                for c in 0..channels {
                    out.push(samples[f * channels + c] * gain);
                }
            }
            out
        } else {
            samples.to_vec()
        };
        if self.fade_in_pos < FADE_IN_BLOCKS {
            self.fade_in_pos += 1;
        }
        out
    }

    /// Shape one SILENCE slot: the FIRST empty slot right after audio is the single
    /// fade-out tail (last audio block ramped `1 → 0`); every later empty slot, and
    /// a silence-only start, is plain zero silence. Enters a silence run.
    pub fn shape_silence(&mut self, channels: usize) -> Vec<f32> {
        let want_tail =
            self.tail_pending && self.seen_audio && channels > 0 && !self.last_block.is_empty();
        self.tail_pending = false;
        self.in_silence = true;
        if want_tail {
            let frames = self.last_block.len() / channels;
            let mut out = Vec::with_capacity(self.last_block.len());
            for f in 0..frames {
                let gain = fade_out_gain(f, frames);
                for c in 0..channels {
                    out.push(self.last_block[f * channels + c] * gain);
                }
            }
            out
        } else {
            vec![0.0f32; self.samples_per_block * channels]
        }
    }
}

/// Linear fade-IN gain for frame `g` (0-based) of a `total`-frame ramp: `0.0` at
/// `g == 0`, exactly `1.0` at `g == total - 1`. A degenerate `total <= 1` is full
/// gain. Callers never pass `g >= total` (the ramp spans exactly `total` frames).
fn fade_in_gain(g: usize, total: usize) -> f32 {
    if total <= 1 {
        return 1.0;
    }
    g as f32 / (total - 1) as f32
}

/// Linear fade-OUT gain for frame `f` (0-based) of a `frames`-frame tail block:
/// exactly `1.0` at `f == 0`, exactly `0.0` at `f == frames - 1`. A degenerate
/// `frames <= 1` is silence.
fn fade_out_gain(f: usize, frames: usize) -> f32 {
    if frames <= 1 {
        return 0.0;
    }
    (frames - 1 - f) as f32 / frames as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Production block size (frames per channel).
    const SPB: usize = 1600;

    fn stereo(v: f32) -> Vec<f32> {
        vec![v; SPB * 2]
    }

    // ── pure gain functions ──────────────────────────────────────────────────

    #[test]
    fn fade_in_gain_hits_exact_endpoints_over_three_blocks() {
        let total = 3 * SPB; // 4800
        assert_eq!(fade_in_gain(0, total), 0.0, "first frame is silence");
        assert_eq!(
            fade_in_gain(total - 1, total),
            1.0,
            "reaches exactly 1.0 at the end of block 3"
        );
        // Linear midpoint (kills +/-/* mutants on the ratio).
        let mid = fade_in_gain(total / 2, total);
        assert!(
            (mid - (2400.0f32 / 4799.0f32)).abs() < 1e-6,
            "linear midpoint, got {mid}"
        );
    }

    #[test]
    fn fade_in_gain_is_continuous_across_the_block_boundary() {
        let total = 3 * SPB;
        // The last frame of block 0 (g = 1599) and the first of block 1 (g = 1600)
        // are one ramp step apart — no discontinuity.
        let end0 = fade_in_gain(SPB - 1, total);
        let start1 = fade_in_gain(SPB, total);
        assert!(start1 > end0);
        assert!((start1 - end0 - (1.0f32 / 4799.0f32)).abs() < 1e-6);
    }

    #[test]
    fn fade_in_gain_degenerate_ramp_is_full_gain() {
        assert_eq!(fade_in_gain(0, 1), 1.0);
        assert_eq!(fade_in_gain(0, 0), 1.0);
    }

    #[test]
    fn fade_out_gain_hits_exact_endpoints_over_one_block() {
        assert_eq!(
            fade_out_gain(0, SPB),
            1.0,
            "tail first frame is full content"
        );
        assert_eq!(
            fade_out_gain(SPB - 1, SPB),
            0.0,
            "tail last frame is silence"
        );
        let mid = fade_out_gain(SPB / 2, SPB);
        assert!(
            (mid - (799.0f32 / 1599.0f32)).abs() < 1e-6,
            "linear midpoint, got {mid}"
        );
    }

    #[test]
    fn fade_out_gain_degenerate_block_is_silence() {
        assert_eq!(fade_out_gain(0, 1), 0.0);
        assert_eq!(fade_out_gain(0, 0), 0.0);
    }

    // ── EdgeFade state machine ───────────────────────────────────────────────

    #[test]
    fn silence_only_history_emits_pure_silence_no_tail() {
        let mut ef = EdgeFade::new(SPB);
        for _ in 0..5 {
            let s = ef.shape_silence(2);
            assert_eq!(s.len(), SPB * 2);
            assert!(
                s.iter().all(|&x| x == 0.0),
                "no tail before any audio played"
            );
        }
    }

    #[test]
    fn fade_out_tail_after_audio_then_plain_silence() {
        let mut ef = EdgeFade::new(SPB);
        // One audio block establishes the last-block source.
        ef.shape_audio(&stereo(1.0), 2);
        // The first empty slot is the fade-out tail: full-content first frame,
        // silent last frame, from the raw (pre-fade) last block.
        let tail = ef.shape_silence(2);
        assert_eq!(tail.len(), SPB * 2);
        assert_eq!(tail[0], 1.0, "tail first frame ≈ 1.0× content");
        assert_eq!(tail[1], 1.0, "both channels of the first frame");
        assert_eq!(tail[(SPB - 1) * 2], 0.0, "tail last frame → 0");
        assert_eq!(tail[(SPB - 1) * 2 + 1], 0.0);
        // A middle frame is between the endpoints (a real ramp, not a cut).
        let midv = tail[(SPB / 2) * 2];
        assert!(midv > 0.0 && midv < 1.0, "linear ramp midpoint, got {midv}");
        // Every later empty slot is plain zero silence.
        let after = ef.shape_silence(2);
        assert!(after.iter().all(|&x| x == 0.0), "only one tail block");
    }

    #[test]
    fn fade_in_ramps_the_first_three_blocks_after_silence_then_full_gain() {
        let mut ef = EdgeFade::new(SPB);
        // Enter a silence run so the next audio starts a fade-in.
        ef.shape_silence(2);
        let out0 = ef.shape_audio(&stereo(1.0), 2);
        let out1 = ef.shape_audio(&stereo(1.0), 2);
        let out2 = ef.shape_audio(&stereo(1.0), 2);
        let out3 = ef.shape_audio(&stereo(1.0), 2);

        // Block 0 starts at silence.
        assert_eq!(out0[0], 0.0, "fade-in starts at 0");
        // Blocks 1 and 2 are still ramping (< full gain) — this is what fails when
        // FADE_IN_BLOCKS is too small.
        assert!(out1[0] < 1.0, "block 1 is still ramping in");
        assert!(out2[0] < 1.0, "block 2 is still ramping in");
        // Exact ramp values (content 1.0 × gain), continuous across the 3 blocks.
        assert!((out1[0] - (1600.0f32 / 4799.0f32)).abs() < 1e-6);
        assert!((out2[0] - (3200.0f32 / 4799.0f32)).abs() < 1e-6);
        // The ramp reaches exactly 1.0 at the end of block 3.
        assert_eq!(
            out2[(SPB - 1) * 2],
            1.0,
            "cumulative ramp is 1.0 at the end"
        );
        // Block 4 (past the fade-in) is full gain.
        assert!(
            out3.iter().all(|&x| x == 1.0),
            "past the fade-in = full gain"
        );
    }

    #[test]
    fn a_channel_change_worth_of_audio_re_arms_a_fresh_tail_and_fade_in() {
        let mut ef = EdgeFade::new(SPB);
        // audio → tail → silence, then audio again gets a NEW fade-in + tail.
        ef.shape_audio(&stereo(1.0), 2);
        let tail1 = ef.shape_silence(2);
        assert_eq!(tail1[0], 1.0);
        ef.shape_silence(2); // plain silence
        // Resume: a fresh fade-in (block 0 starts at 0) and a fresh tail.
        let resumed = ef.shape_audio(&stereo(0.5), 2);
        assert_eq!(resumed[0], 0.0, "the resumed run fades in again from 0");
        let tail2 = ef.shape_silence(2);
        assert_eq!(tail2[0], 0.5, "the new tail uses the most recent block");
    }
}
