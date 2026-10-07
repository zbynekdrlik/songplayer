//! #233: a VBAN destination's rate conversion. The program is 48 kHz; a
//! destination at another rate gets each boundary's block through rubato's
//! synchronous FFT resampler with BOTH sides fixed: one 1600-frame block in,
//! exactly `rate / 30` frames out (the packet schedule needs whole packets per
//! boundary; rubato's `Async` output varies by a frame). The 48 kHz
//! destination (FOH's) is passed through untouched: no copy, no filter, the
//! #210 bytes. A silent block still goes through the filter (its state stays
//! continuous). The delay is half the block FFT (`rate / 60` frames, 16.7 ms).
//! If rubato refuses the converter (never for a supported rate) the
//! destination sends silence and says why (`failed`).

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};
use sp_core::audio_outputs::PROGRAM_RATE;
use sp_core::genlock::GENLOCK_GRID_FPS;

use crate::playback::vban_packet::{VBAN_BLOCK_FRAMES, VBAN_BLOCK_SAMPLES, VBAN_CHANNELS};

/// The converter's delay at `rate_hz`, frames of the destination's rate.
pub fn fft_delay_frames(rate_hz: u32) -> usize {
    if rate_hz == PROGRAM_RATE {
        0
    } else {
        rate_hz as usize / 60
    }
}

/// One destination's converter (owned by its VBAN thread's sender).
pub struct VbanRateConverter {
    fft: Option<Fft<f32>>,
    out: Vec<f32>,
    zeros: Vec<f32>,
    out_frames: usize,
    failed: Option<String>,
}

impl VbanRateConverter {
    pub fn new(rate_hz: u32) -> Self {
        let bypass = Self {
            fft: None,
            out: Vec::new(),
            zeros: Vec::new(),
            out_frames: 0,
            failed: None,
        };
        if rate_hz == PROGRAM_RATE {
            return bypass;
        }
        let out_frames = (i64::from(rate_hz) / GENLOCK_GRID_FPS) as usize;
        let built = Fft::<f32>::new(
            PROGRAM_RATE as usize,
            rate_hz as usize,
            VBAN_BLOCK_FRAMES,
            VBAN_CHANNELS,
            FixedSync::Both,
        )
        .map_err(|e| e.to_string())
        .and_then(|fft| {
            let (frames_in, frames_out) = (fft.input_frames_next(), fft.output_frames_next());
            if frames_in == VBAN_BLOCK_FRAMES && frames_out == out_frames {
                Ok(fft)
            } else {
                Err(format!(
                    "the {rate_hz} Hz converter takes {frames_in} frames for {frames_out}"
                ))
            }
        });
        match built {
            Ok(fft) => Self {
                fft: Some(fft),
                out: vec![0.0; out_frames * VBAN_CHANNELS],
                zeros: vec![0.0; VBAN_BLOCK_SAMPLES],
                out_frames,
                failed: None,
            },
            Err(e) => Self {
                failed: Some(e),
                ..bypass
            },
        }
    }

    /// One boundary's block (`None` = silence) at the destination's rate:
    /// the block itself at 48 kHz, else the converted frames (`None` only
    /// when the converter failed). A block that is not one program block is
    /// converted as silence.
    pub fn convert<'a>(&'a mut self, block: Option<&'a [f32]>) -> Option<&'a [f32]> {
        if self.failed.is_some() {
            return None;
        }
        let Self {
            fft,
            out,
            zeros,
            out_frames,
            ..
        } = self;
        let Some(fft) = fft.as_mut() else {
            return block;
        };
        let input = block
            .filter(|b| b.len() == VBAN_BLOCK_SAMPLES)
            .unwrap_or(zeros.as_slice());
        {
            let adapter_in = InterleavedSlice::new(input, VBAN_CHANNELS, VBAN_BLOCK_FRAMES).ok()?;
            let mut adapter_out =
                InterleavedSlice::new_mut(&mut out[..], VBAN_CHANNELS, *out_frames).ok()?;
            fft.process_into_buffer(&adapter_in, &mut adapter_out, None)
                .ok()?;
        }
        Some(&out[..])
    }

    /// The converter's delay, frames at the destination's rate (0 at 48 kHz).
    pub fn delay_frames(&self) -> usize {
        self.fft.as_ref().map_or(0, |f| f.output_delay())
    }

    /// Why the converter could not be built (the destination sends silence).
    pub fn failed(&self) -> Option<&str> {
        self.failed.as_deref()
    }
}

#[cfg(test)]
#[path = "vban_rate_tests.rs"]
mod tests;
