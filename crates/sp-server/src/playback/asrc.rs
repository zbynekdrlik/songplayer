//! #233: the ASIO output's resampler and its re-centre.
//!
//! `Asrc`: ONE rubato `Async` sinc stage (256 taps, BlackmanHarris², fixed
//! input of one 1600-frame program block) converts the 48 kHz program to the
//! card's rate; the servo (`asrc_servo.rs`) sets its relative ratio
//! `1 + ppm·1e-6` once per block, ramped across the block. Allocation-free
//! after `new` (`process_into_buffer` into its own buffer; rubato's `log`
//! feature is off). Its delay is `sinc_len · ratio / 2` frames (256 at
//! 96 kHz, 2.7 ms).
//!
//! `Splice`: the servo's re-centre on the resampler's output (the card's
//! rate): a 5 ms fade out, the inserted silence or the skipped frames, a 5 ms
//! fade in — never a click. It holds back its last 5 ms so a fade-out can
//! still reach audio not yet in the ring (a constant 5 ms of latency, counted
//! in the servo's `buffered_frames`).

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Adjustable, Async, FixedAsync, Resampler, SincInterpolationParameters, WindowFunction,
};

use crate::playback::vban_packet::{
    VBAN_BLOCK_FRAMES, VBAN_BLOCK_SAMPLES, VBAN_CHANNELS, VBAN_SAMPLE_RATE_HZ,
};

/// The sinc length (rubato's default; ~2.7 ms at 96 kHz).
pub const ASRC_SINC_LEN: usize = 256;
/// The ratio's room around nominal: ±1000 ppm, well past the servo's ±300.
pub const ASRC_MAX_RELATIVE: f64 = 1.001;
/// A re-centre's fade, s.
pub const SPLICE_FADE_S: f64 = 0.005;

/// The program (48 kHz, stereo, one 1600-frame block per boundary) at the
/// card's rate, with the servo's correction.
pub struct Asrc {
    inner: Async<f32>,
    out: Vec<f32>,
}

impl Asrc {
    pub fn new(device_rate_hz: f64) -> Result<Self, String> {
        let ratio = device_rate_hz / VBAN_SAMPLE_RATE_HZ as f64;
        let params =
            SincInterpolationParameters::new(ASRC_SINC_LEN, WindowFunction::BlackmanHarris2);
        let inner = Async::<f32>::new_sinc(
            ratio,
            ASRC_MAX_RELATIVE,
            &params,
            VBAN_BLOCK_FRAMES,
            VBAN_CHANNELS,
            FixedAsync::Input,
        )
        .map_err(|e| e.to_string())?;
        let out = vec![0.0; inner.output_frames_max() * VBAN_CHANNELS];
        Ok(Self { inner, out })
    }

    /// The servo's correction (ramped across the next block).
    pub fn set_correction_ppm(&mut self, ppm: f64) -> Result<(), String> {
        self.inner
            .set_resample_ratio_relative(1.0 + ppm * 1e-6, true)
            .map_err(|e| e.to_string())
    }

    /// One program block (3200 interleaved samples) at the card's rate.
    pub fn process(&mut self, block: &[f32]) -> Result<&[f32], String> {
        if block.len() != VBAN_BLOCK_SAMPLES {
            return Err(format!(
                "a block of {} samples, not {VBAN_BLOCK_SAMPLES}",
                block.len()
            ));
        }
        let frames_out = self.inner.output_frames_max();
        let produced = {
            let input = InterleavedSlice::new(block, VBAN_CHANNELS, VBAN_BLOCK_FRAMES)
                .map_err(|e| e.to_string())?;
            let mut output =
                InterleavedSlice::new_mut(&mut self.out[..], VBAN_CHANNELS, frames_out)
                    .map_err(|e| e.to_string())?;
            self.inner
                .process_into_buffer(&input, &mut output, None)
                .map_err(|e| e.to_string())?
                .1
        };
        Ok(&self.out[..produced * VBAN_CHANNELS])
    }

    /// The resampler's delay, in frames at the card's rate.
    pub fn delay_frames(&self) -> usize {
        self.inner.output_delay()
    }

    /// The most frames one block can give.
    pub fn max_out_frames(&self) -> usize {
        self.inner.output_frames_max()
    }
}

/// The re-centre (module doc). Samples are interleaved stereo.
pub struct Splice {
    fade: usize,
    hold: Vec<f32>,
    out: Vec<f32>,
    insert: usize,
    skip: usize,
    fade_in_left: usize,
    muted: bool,
}

impl Splice {
    /// For a card at `device_rate_hz`; `max_insert_frames` and
    /// `max_block_frames` size the buffer once (no allocation per block).
    pub fn new(device_rate_hz: f64, max_insert_frames: usize, max_block_frames: usize) -> Self {
        let fade = ((device_rate_hz * SPLICE_FADE_S).round() as usize).max(1);
        Self {
            fade,
            hold: vec![0.0; fade * VBAN_CHANNELS],
            out: Vec::with_capacity((fade + max_insert_frames + max_block_frames) * VBAN_CHANNELS),
            insert: 0,
            skip: 0,
            fade_in_left: 0,
            muted: false,
        }
    }

    /// The frames held back (counted as buffered).
    pub fn held_frames(&self) -> usize {
        self.fade
    }

    /// Silence before the next audio (the servo's positive re-centre).
    pub fn insert(&mut self, frames: usize) {
        self.insert += frames;
    }

    /// Frames of the coming audio dropped (the servo's negative re-centre).
    pub fn skip(&mut self, frames: usize) {
        self.skip += frames;
    }

    /// One block of the resampler's output → what goes to the ring now.
    pub fn process(&mut self, input: &[f32]) -> &[f32] {
        let fade = self.fade;
        self.out.clear();
        if (self.insert > 0 || self.skip > 0) && !self.muted {
            for (i, frame) in self.hold.chunks_exact_mut(VBAN_CHANNELS).enumerate() {
                let g = (fade - i - 1) as f32 / fade as f32;
                frame.iter_mut().for_each(|x| *x *= g);
            }
            self.muted = true;
        }
        self.out.extend_from_slice(&self.hold);
        self.out
            .resize(self.out.len() + self.insert * VBAN_CHANNELS, 0.0);
        self.insert = 0;
        let skipped = (self.skip * VBAN_CHANNELS).min(input.len());
        self.skip -= skipped / VBAN_CHANNELS;
        let rest = &input[skipped..];
        let start = self.out.len();
        self.out.extend_from_slice(rest);
        if self.muted && self.skip == 0 && !rest.is_empty() {
            self.muted = false;
            self.fade_in_left = fade;
        }
        for frame in self.out[start..].chunks_exact_mut(VBAN_CHANNELS) {
            if self.fade_in_left == 0 {
                break;
            }
            let g = (fade - self.fade_in_left + 1) as f32 / fade as f32;
            frame.iter_mut().for_each(|x| *x *= g);
            self.fade_in_left -= 1;
        }
        let keep = fade * VBAN_CHANNELS;
        let n = self.out.len();
        self.hold.copy_from_slice(&self.out[n - keep..]);
        self.out.truncate(n - keep);
        &self.out
    }
}

#[cfg(test)]
#[path = "asrc_tests.rs"]
mod tests;
