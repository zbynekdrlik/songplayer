//! #233: a VBAN destination's rate conversion. The program is 48 kHz; a
//! destination at another rate gets each boundary's block through
//! dstack-resample's sinc converter (#285 step 3: one conversion library for
//! every app; 256 taps, the table oversampled 256×, BlackmanHarris², cubic)
//! and gives exactly `rate / 30` frames per boundary (the packet schedule
//! needs whole packets per boundary). The converter's own output varies by a
//! frame around that, so it feeds a FIFO that the destination drains by
//! exactly `rate / 30`. The 48 kHz destination (FOH's) is passed through
//! untouched: no copy, no filter, the #210 bytes. A silent block still goes
//! through the filter (its state stays continuous).
//!
//! The delay stays `rate / 60` frames (16.7 ms), the rubato `Fft` path's
//! (ROZHODNUTÉ with camera-box, #285, 3.11.2026: the CG song's audio and its
//! NDI video line up at the venue through that timing). The FIFO starts with
//! `rate / 60` minus the converter's own delay frames of silence. The
//! converter's delay is 2.67 ms at 96 kHz, so it never needs that whole
//! budget.
//!
//! #289: with `audio_low_latency` on ([`VbanRateConverter::for_latency`])
//! the FIFO starts empty: the delay is the converter's own
//! (`Converter::delay_frames`, 2.67 ms at 96 kHz), never padded to
//! `rate / 60`.
//!
//! If dstack refuses the converter (never for a supported rate:
//! `vban_rate_tests` builds each) the destination sends silence, and its VBAN
//! thread logs one WARN naming why (`failed`, read by `VbanSender`); a block
//! the converter refuses also goes out as silence.

use std::collections::VecDeque;

use dstack_resample::{Converter, Fixed};
use sp_core::audio_outputs::PROGRAM_RATE;
use sp_core::genlock::GENLOCK_GRID_FPS;

use crate::playback::vban_packet::{VBAN_BLOCK_FRAMES, VBAN_BLOCK_SAMPLES, VBAN_CHANNELS};

/// Whether a destination at `rate_hz` takes the program's blocks as they are
/// (48 kHz: no converter, #210's bytes). Its own fn, not inside `new`:
/// cargo-mutants never mutates a fn named `new` (FOH's bypass stays gated).
pub fn passes_through(rate_hz: u32) -> bool {
    rate_hz == PROGRAM_RATE
}

/// Frames of one boundary's block at `rate_hz` (`rate / 30`).
pub fn block_frames_at(rate_hz: u32) -> usize {
    (i64::from(rate_hz) / GENLOCK_GRID_FPS) as usize
}

/// The destination's conversion delay at `rate_hz`, frames of its rate:
/// half a block (`rate / 60`, 16.7 ms), 0 at 48 kHz.
pub fn fft_delay_frames(rate_hz: u32) -> usize {
    if passes_through(rate_hz) {
        0
    } else {
        block_frames_at(rate_hz) / 2
    }
}

/// The silence the FIFO starts with: the destination's delay less the
/// converter's own (never negative).
pub fn fifo_prime_frames(rate_hz: u32, converter_delay: usize) -> usize {
    fft_delay_frames(rate_hz).saturating_sub(converter_delay)
}

/// #289: the silence the FIFO starts with: [`fifo_prime_frames`] with
/// `audio_low_latency` off, none with it on (the delay is then the
/// converter's own).
pub fn prime_frames(rate_hz: u32, converter_delay: usize, low_latency: bool) -> usize {
    if low_latency {
        0
    } else {
        fifo_prime_frames(rate_hz, converter_delay)
    }
}

/// #289: the conversion delay of a destination at `rate_hz` with
/// `audio_low_latency` on, frames of its rate, as its sender runs it: the
/// converter's own delay plus the first block's top-up (260 frames =
/// 2.708 ms at 96 kHz), 0 at 48 kHz. Measured once per rate on a real
/// converter (one silent block) and kept, for the status's `latency_ms`.
pub fn low_latency_delay_frames(rate_hz: u32) -> usize {
    use std::sync::Mutex;
    static MEASURED: Mutex<Vec<(u32, usize)>> = Mutex::new(Vec::new());
    let mut measured = MEASURED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&(_, frames)) = measured.iter().find(|(r, _)| *r == rate_hz) {
        return frames;
    }
    let mut converter = VbanRateConverter::for_latency(rate_hz, true);
    converter.convert(None);
    let frames = converter.delay_frames();
    measured.push((rate_hz, frames));
    frames
}

/// The converter and its exact-size output.
struct Conversion {
    converter: Converter,
    /// One block, channel-major `f64`.
    input: Vec<f64>,
    /// The converter's output, channel-major, `max_out` frames a channel.
    output: Vec<f64>,
    max_out: usize,
    /// Converted frames not yet sent, interleaved.
    fifo: VecDeque<f32>,
    /// What [`VbanRateConverter::convert`] gives: `out_frames`, interleaved.
    out: Vec<f32>,
    out_frames: usize,
    /// The silence the FIFO started with, frames (#289: and every top-up).
    primed: usize,
    /// #289 (`audio_low_latency` on): a FIFO short of a block is topped up
    /// with leading silence instead of sending silence for the block.
    top_up: bool,
}

/// One destination's converter (owned by its VBAN thread's sender).
pub struct VbanRateConverter {
    conversion: Option<Conversion>,
    failed: Option<String>,
}

impl VbanRateConverter {
    /// The converter of a destination at `rate_hz`, padded to `rate / 60`
    /// (#285's timing, `audio_low_latency` off).
    pub fn new(rate_hz: u32) -> Self {
        Self::for_latency(rate_hz, false)
    }

    /// #289: the converter of a destination at `rate_hz`: padded to
    /// `rate / 60` with `low_latency` off ([`VbanRateConverter::new`]), else
    /// only the converter's own delay ([`prime_frames`]).
    pub fn for_latency(rate_hz: u32, low_latency: bool) -> Self {
        if passes_through(rate_hz) {
            return Self {
                conversion: None,
                failed: None,
            };
        }
        match Converter::new(
            PROGRAM_RATE,
            rate_hz,
            VBAN_CHANNELS,
            VBAN_BLOCK_FRAMES,
            Fixed::Input,
        ) {
            Ok(converter) => Self {
                conversion: Some(Conversion::of(converter, rate_hz, low_latency)),
                failed: None,
            },
            Err(e) => Self::refused(&e.to_string()),
        }
    }

    /// One boundary's block (`None` = silence) at the destination's rate:
    /// the block itself at 48 kHz, else exactly `rate / 30` converted frames
    /// (`None` only when the converter failed). A block that is not one
    /// program block is converted as silence.
    pub fn convert<'a>(&'a mut self, block: Option<&'a [f32]>) -> Option<&'a [f32]> {
        if self.failed.is_some() {
            return None;
        }
        let Some(c) = self.conversion.as_mut() else {
            return block;
        };
        c.convert(block.filter(|b| b.len() == VBAN_BLOCK_SAMPLES))
    }

    /// The conversion's delay, frames at the destination's rate (0 at
    /// 48 kHz): the FIFO's starting silence plus the converter's own.
    pub fn delay_frames(&self) -> usize {
        self.conversion
            .as_ref()
            .map_or(0, |c| c.converter.delay_frames() + c.primed)
    }

    /// Why the converter could not be built (the destination sends silence).
    pub fn failed(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    /// A converter that was refused (`why`): the destination sends silence
    /// (no supported rate refuses one; the tests build it directly).
    pub fn refused(why: &str) -> Self {
        Self {
            conversion: None,
            failed: Some(why.to_string()),
        }
    }
}

impl Conversion {
    fn of(converter: Converter, rate_hz: u32, low_latency: bool) -> Self {
        let max_out = converter.output_frames_max();
        let out_frames = block_frames_at(rate_hz);
        let primed = prime_frames(rate_hz, converter.delay_frames(), low_latency);
        let mut fifo = VecDeque::with_capacity((primed + out_frames + max_out) * VBAN_CHANNELS);
        fifo.extend(std::iter::repeat_n(0.0f32, primed * VBAN_CHANNELS));
        Self {
            converter,
            input: vec![0.0; VBAN_BLOCK_SAMPLES],
            output: vec![0.0; max_out * VBAN_CHANNELS],
            max_out,
            fifo,
            out: vec![0.0; out_frames * VBAN_CHANNELS],
            out_frames,
            primed,
            top_up: low_latency,
        }
    }

    fn convert(&mut self, block: Option<&[f32]>) -> Option<&[f32]> {
        let frames = VBAN_BLOCK_FRAMES;
        match block {
            Some(b) => {
                for (i, pair) in b.chunks_exact(VBAN_CHANNELS).enumerate() {
                    for (ch, &x) in pair.iter().enumerate() {
                        self.input[ch * frames + i] = f64::from(x);
                    }
                }
            }
            None => self.input.fill(0.0),
        }
        let (_, written) = self
            .converter
            .process(&self.input, frames, &mut self.output, self.max_out)
            .ok()?;
        for i in 0..written {
            for ch in 0..VBAN_CHANNELS {
                self.fifo
                    .push_back(self.output[ch * self.max_out + i] as f32);
            }
        }
        let want = self.out_frames * VBAN_CHANNELS;
        if self.fifo.len() < want {
            if !self.top_up {
                return None;
            }
            // #289: the converter's first call gives a few frames under a
            // block (4 at 96 kHz); the FIFO starts empty, so that shortfall
            // is put in front once, as silence, and counted in the delay.
            let missing = want - self.fifo.len();
            for _ in 0..missing {
                self.fifo.push_front(0.0);
            }
            self.primed += missing / VBAN_CHANNELS;
        }
        for (o, x) in self.out.iter_mut().zip(self.fifo.drain(..want)) {
            *o = x;
        }
        Some(&self.out[..])
    }
}

#[cfg(test)]
#[path = "vban_rate_tests.rs"]
mod tests;
