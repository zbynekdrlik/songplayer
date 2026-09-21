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
//! It is PURE and single-threaded — [`AudioEmitter::samples_for`] calls
//! [`on_audio`](EdgeFade::on_audio) for every audio block and
//! [`on_silence`](EdgeFade::on_silence) for every silence block, in slot order.
//! To keep the #203 allocation-free contract on the TIME_CRITICAL emit thread the
//! shaped samples go into a REUSED [`out`](EdgeFade) scratch (read back BORROWED
//! via [`shaped`](EdgeFade::shaped)) and only the FADED slots use it — a full-gain
//! audio block and a plain silence block are still returned borrowed straight from
//! the ring's `block_buf` / the emitter's reusable `silence` (no scratch touched).
//! The ring, the grid timecodes and the `silence_blocks` accounting are all
//! UNTOUCHED — the fade only reshapes the interleaved samples handed to NDI.

/// Audio blocks over which the post-silence fade-IN ramps `0 → 1` (3 × 33.3 ms ≈
/// 100 ms, continuous across the three blocks).
pub const FADE_IN_BLOCKS: u32 = 3;

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
    /// fade-in ramp is active, then clamped there (full gain). Starts AT
    /// `FADE_IN_BLOCKS` so a cold start with no preceding silence run plays at full
    /// gain (the fade-in fires only on a genuine silence→audio edge, `on_audio`
    /// resetting it to 0); in production the emitter always emits silence while the
    /// ring fills, so the first real audio still fades in.
    fade_in_pos: u32,
    /// A verbatim copy of the last audio block emitted (pre-fade), reused as the
    /// fade-out tail source. Empty until audio has flowed.
    last_block: Vec<f32>,
    /// Reused shaped-output scratch: holds the fade-in or fade-out block so
    /// [`samples_for`](super::AudioEmitter::samples_for) can return it BORROWED
    /// with no per-slot allocation on the TIME_CRITICAL thread (#203).
    out: Vec<f32>,
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
            // Full gain until a silence run precedes an audio block (see the field
            // doc): a cold start with no preceding silence does not fade in.
            fade_in_pos: FADE_IN_BLOCKS,
            last_block: Vec::new(),
            out: Vec::new(),
            tail_pending: false,
        }
    }

    /// Update state for one AUDIO slot whose raw samples are `raw`. Records the
    /// block (pre-fade) as the fade-out tail source and arms the tail. When the
    /// block falls in the first [`FADE_IN_BLOCKS`] after a silence run, write its
    /// fade-in ramp into the reused `out` scratch and return `true` (read via
    /// [`shaped`](Self::shaped)); otherwise leave `out` untouched and return
    /// `false` — the caller sends `raw` unchanged (full gain, still borrowed from
    /// the ring).
    pub fn on_audio(&mut self, raw: &[f32], channels: usize) -> bool {
        self.seen_audio = true;
        if self.in_silence {
            // Silence → audio edge: restart the fade-in from block 0.
            self.in_silence = false;
            self.fade_in_pos = 0;
        }
        // Save a verbatim copy (reused buffer) as the fade-out tail source.
        self.last_block.clear();
        self.last_block.extend_from_slice(raw);
        self.tail_pending = true;

        let fading = channels > 0 && self.fade_in_pos < FADE_IN_BLOCKS;
        if fading {
            let base = self.fade_in_pos as usize * self.samples_per_block;
            let total = FADE_IN_BLOCKS as usize * self.samples_per_block;
            let frames = raw.len() / channels;
            self.out.clear();
            for f in 0..frames {
                let gain = fade_in_gain(base + f, total);
                for c in 0..channels {
                    self.out.push(raw[f * channels + c] * gain);
                }
            }
        }
        if self.fade_in_pos < FADE_IN_BLOCKS {
            self.fade_in_pos += 1;
        }
        fading
    }

    /// Update state for one SILENCE slot. When it is the FIRST empty slot right
    /// after audio, write the fade-out tail (the last audio block ramped `1 → 0`)
    /// into the reused `out` scratch and return `true` (read via
    /// [`shaped`](Self::shaped)); otherwise return `false` — the caller sends plain
    /// zero silence. Enters a silence run.
    pub fn on_silence(&mut self, channels: usize) -> bool {
        let want_tail =
            self.tail_pending && self.seen_audio && channels > 0 && !self.last_block.is_empty();
        self.tail_pending = false;
        self.in_silence = true;
        if want_tail {
            let frames = self.last_block.len() / channels;
            self.out.clear();
            for f in 0..frames {
                let gain = fade_out_gain(f, frames);
                for c in 0..channels {
                    self.out.push(self.last_block[f * channels + c] * gain);
                }
            }
        }
        want_tail
    }

    /// The shaped block written by the most recent `on_audio` / `on_silence` that
    /// returned `true`. Only valid immediately after such a call.
    pub fn shaped(&self) -> &[f32] {
        &self.out
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
    (frames - 1 - f) as f32 / (frames - 1) as f32
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
            assert!(!ef.on_silence(2), "no tail before any audio played");
        }
    }

    #[test]
    fn fade_out_tail_after_audio_then_plain_silence() {
        let mut ef = EdgeFade::new(SPB);
        // One audio block establishes the last-block source.
        ef.on_audio(&stereo(1.0), 2);
        // The first empty slot is the fade-out tail (from the raw, pre-fade block).
        assert!(ef.on_silence(2), "first empty slot after audio is the tail");
        let tail = ef.shaped();
        assert_eq!(tail.len(), SPB * 2);
        assert_eq!(tail[0], 1.0, "tail first frame ≈ 1.0× content");
        assert_eq!(tail[1], 1.0, "both channels of the first frame");
        assert_eq!(tail[(SPB - 1) * 2], 0.0, "tail last frame → 0");
        assert_eq!(tail[(SPB - 1) * 2 + 1], 0.0);
        let midv = tail[(SPB / 2) * 2];
        assert!(midv > 0.0 && midv < 1.0, "linear ramp midpoint, got {midv}");
        // Every later empty slot is plain zero silence (no tail).
        assert!(!ef.on_silence(2), "only one tail block");
    }

    #[test]
    fn fade_in_ramps_the_first_three_blocks_after_silence_then_full_gain() {
        let mut ef = EdgeFade::new(SPB);
        // Enter a silence run so the next audio starts a fade-in.
        ef.on_silence(2);

        // Block 0 — fades in, starting at silence.
        assert!(ef.on_audio(&stereo(1.0), 2), "block 0 fades in");
        assert_eq!(ef.shaped()[0], 0.0, "fade-in starts at 0");

        // Block 1 — still ramping (this is what fails when FADE_IN_BLOCKS is small).
        assert!(ef.on_audio(&stereo(1.0), 2), "block 1 is still ramping in");
        let out1_first = ef.shaped()[0];
        assert!(out1_first < 1.0, "block 1 below full gain");
        assert!((out1_first - (1600.0f32 / 4799.0f32)).abs() < 1e-6);

        // Block 2 — still ramping, and the ramp reaches exactly 1.0 at its end.
        assert!(ef.on_audio(&stereo(1.0), 2), "block 2 is still ramping in");
        let out2_first = ef.shaped()[0];
        let out2_last = ef.shaped()[(SPB - 1) * 2];
        assert!(out2_first < 1.0, "block 2 below full gain at its start");
        assert!((out2_first - (3200.0f32 / 4799.0f32)).abs() < 1e-6);
        assert_eq!(
            out2_last, 1.0,
            "cumulative ramp is 1.0 at the end of block 3"
        );

        // Block 3 — past the fade-in: full gain (pass-through, no scratch).
        assert!(
            !ef.on_audio(&stereo(1.0), 2),
            "past the fade-in = full gain"
        );
    }

    #[test]
    fn a_resumed_run_re_arms_a_fresh_tail_and_fade_in() {
        let mut ef = EdgeFade::new(SPB);
        // audio → tail → silence, then audio again gets a NEW fade-in + tail.
        ef.on_audio(&stereo(1.0), 2);
        assert!(ef.on_silence(2));
        assert_eq!(ef.shaped()[0], 1.0);
        assert!(!ef.on_silence(2)); // plain silence
        // Resume: a fresh fade-in (block 0 starts at 0) and a fresh tail.
        assert!(
            ef.on_audio(&stereo(0.5), 2),
            "the resumed run fades in again"
        );
        assert_eq!(ef.shaped()[0], 0.0, "resumed fades in from 0");
        assert!(ef.on_silence(2));
        assert_eq!(
            ef.shaped()[0],
            0.5,
            "the new tail uses the most recent block"
        );
    }
}
