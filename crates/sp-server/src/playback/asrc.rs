//! #233: the ASIO output's resampler and its re-centre.
//!
//! `Asrc`: an adapter over dstack-resample's drifting converter
//! (`dstack_resample::Converter::drifting`, #285 step 3: the conversion is
//! dstack's, done once and measured once for every app). ONE rubato `Async`
//! band-limited sinc stage converts the 48 kHz program to the card's rate,
//! fixed input of one 1600-frame program block. dstack's core runs in `f64`,
//! channel-major; this adapter takes and gives SongPlayer's interleaved `f32`
//! stereo, through buffers sized once. dstack-resample is the crate that
//! names rubato's generic `Async::<f64>`, so the workspace compiles it (and
//! rubato's dot kernels) at `opt-level = 3` even in tests.
//! Its filter is the lane's measured choice, now dstack's `PARAMS`
//! ([`asrc_params`], pinned by a test):
//! - 256 taps;
//! - the sinc table oversampled 256× (twice rubato's default);
//! - BlackmanHarris²;
//! - cubic interpolation between the table's rows;
//! - the automatic cutoff: 0.947 of the lower Nyquist, 22.7 kHz at 48 → 96 kHz.
//!
//! The servo (`asrc_servo.rs`) sets its relative ratio `1 + ppm·1e-6` once
//! per block, ramped across the block: that ratio absorbs every difference
//! (the owner's ruling of 8.10.2026). The splice below is used only to prime
//! the ring and for a hard re-centre. A correction outside
//! `1/1.001 ..= 1.001` is refused here (dstack would clamp it to ±1000 ppm).
//!
//! Its delay is `sinc_len · ratio / 2` frames: 256 at 96 kHz, 2.7 ms (the
//! oversampling does not change it).
//!
//! Measured through it, 48 → 96 kHz (`asrc_tests.rs`, the figures in the CI
//! log):
//! - a 1 kHz tone at −1 dBFS, at −300, 0 and +300 ppm: THD+N ≥ 120 dB;
//! - a 20 kHz tone: nothing above −120 dBFS between 24 and 48 kHz.
//!
//! Allocation-free after `new` (dstack's `process` allocates nothing; the
//! adapter's buffers are its own).
//!
//! `Splice`: the priming and a hard re-centre on the resampler's output (the
//! card's rate): a 5 ms fade out, the inserted silence or the skipped frames,
//! a 5 ms fade in — never a click. It holds back its last 5 ms so a fade-out
//! can still reach audio not yet in the ring (a constant 5 ms of latency,
//! counted in the servo's `buffered_frames`). dstack has no splice: it stays
//! SongPlayer's.

use dstack_resample::{Converter, Fixed};
use rubato::SincInterpolationParameters;
use sp_core::audio_outputs::PROGRAM_RATE;

use crate::playback::vban_packet::{VBAN_BLOCK_FRAMES, VBAN_BLOCK_SAMPLES, VBAN_CHANNELS};

/// The sinc length (~2.7 ms at 96 kHz): dstack's.
pub const ASRC_SINC_LEN: usize = dstack_resample::TAPS;
/// The sinc table's oversampling: dstack's, the lane's measured choice
/// (twice rubato's default of 128).
pub const ASRC_OVERSAMPLING: usize = dstack_resample::OVERSAMPLING;
/// The ratio's room around nominal that this adapter accepts: ±1000 ppm,
/// well past the servo's ±300 (`1/1.001 ..= 1.001`, so −1000 ppm itself is
/// refused).
pub const ASRC_MAX_RELATIVE: f64 = 1.001;
/// A re-centre's fade, s.
pub const SPLICE_FADE_S: f64 = 0.005;

/// The resampler's filter (the module doc): dstack-resample's `PARAMS`.
pub fn asrc_params() -> SincInterpolationParameters {
    dstack_resample::PARAMS
}

/// The program (48 kHz, stereo, one 1600-frame block per boundary) at the
/// card's rate, with the servo's correction.
pub struct Asrc {
    inner: Converter,
    /// One block, channel-major `f64`.
    input: Vec<f64>,
    /// The converter's output, channel-major, `max_out` frames a channel.
    output: Vec<f64>,
    max_out: usize,
    /// What [`Asrc::process`] gives: interleaved `f32`.
    out: Vec<f32>,
}

impl Asrc {
    pub fn new(device_rate_hz: f64) -> Result<Self, String> {
        let inner = Converter::drifting(
            PROGRAM_RATE,
            device_rate_hz.round() as u32,
            VBAN_CHANNELS,
            VBAN_BLOCK_FRAMES,
            Fixed::Input,
        )
        .map_err(|e| e.to_string())?;
        let max_out = inner.output_frames_max();
        Ok(Self {
            inner,
            input: vec![0.0; VBAN_BLOCK_SAMPLES],
            output: vec![0.0; max_out * VBAN_CHANNELS],
            max_out,
            out: vec![0.0; max_out * VBAN_CHANNELS],
        })
    }

    /// The servo's correction (ramped across the next block); outside
    /// [`ASRC_MAX_RELATIVE`] refused.
    pub fn set_correction_ppm(&mut self, ppm: f64) -> Result<(), String> {
        let relative = 1.0 + ppm * 1e-6;
        if !(1.0 / ASRC_MAX_RELATIVE..=ASRC_MAX_RELATIVE).contains(&relative) {
            return Err(format!(
                "a correction of {ppm} ppm is outside the resampler's room"
            ));
        }
        self.inner.set_ppm(ppm).map_err(|e| e.to_string())
    }

    /// One program block (3200 interleaved samples) at the card's rate.
    pub fn process(&mut self, block: &[f32]) -> Result<&[f32], String> {
        if block.len() != VBAN_BLOCK_SAMPLES {
            return Err(format!(
                "a block of {} samples, not {VBAN_BLOCK_SAMPLES}",
                block.len()
            ));
        }
        let frames = VBAN_BLOCK_FRAMES;
        for (i, frame) in block.chunks_exact(VBAN_CHANNELS).enumerate() {
            for (c, &x) in frame.iter().enumerate() {
                self.input[c * frames + i] = f64::from(x);
            }
        }
        let (_, produced) = self
            .inner
            .process(&self.input, frames, &mut self.output, self.max_out)
            .map_err(|e| e.to_string())?;
        let max_out = self.max_out;
        let samples = produced * VBAN_CHANNELS;
        for (i, frame) in self.out[..samples]
            .chunks_exact_mut(VBAN_CHANNELS)
            .enumerate()
        {
            for (c, y) in frame.iter_mut().enumerate() {
                *y = self.output[c * max_out + i] as f32;
            }
        }
        Ok(&self.out[..samples])
    }

    /// The resampler's delay, in frames at the card's rate.
    pub fn delay_frames(&self) -> usize {
        self.inner.delay_frames()
    }

    /// The most frames one block can give.
    pub fn max_out_frames(&self) -> usize {
        self.max_out
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
    /// The servo's largest insert is its start re-centre, up to the target
    /// latency (`asrc_servo::BASE_LATENCY_100NS` + the entry's delay) in
    /// frames; a bigger one reallocates once, on the worker thread.
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

    /// Frames still to skip: a skip longer than one block runs over several
    /// (the servo's observation counts them out of the buffered frames).
    pub fn pending_skip_frames(&self) -> usize {
        self.skip
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
