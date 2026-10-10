//! #233: one rubato `Async` band-limited sinc stage behind a non-generic
//! type: interleaved `f32`, a fixed input of `frames_in` frames per call
//! (`FixedAsync::Input`), the filter passed in.
//!
//! Why a crate of its own: every hot path of rubato's sinc stage is generic
//! (`Async<T>`, `InnerSinc<T>`, the interpolators, the `audioadapter`
//! slices), so it is compiled in the crate that names `Async::<f32>`. That is
//! this one, which the workspace compiles at `opt-level = 3` in every profile
//! (`[profile.dev.package.sp-asrc]`; test and mutants inherit dev). Before,
//! sp-server named it, unoptimized in its tests: a 256-tap cubic stage took
//! tens of ms per 1600-frame block, and the ASIO tests that push thousands
//! of blocks took 20–110 s each (#233). sp-server calls these functions as
//! ordinary ones, so its own builds (a mutant's included) never recompile
//! them: nothing here is `#[inline]` or generic, on purpose. The other half
//! is rubato's own: its AVX / SSE / NEON dot kernels are plain functions,
//! compiled in rubato, which the workspace optimizes too
//! (`[profile.dev.package.rubato]`; measured: sp-asrc alone took the slowest
//! ASIO test only from 46 s to 31 s).
//!
//! What the stage is for, its filter and its measurements are sp-server's
//! `playback::asrc::Asrc`'s to say; this crate only carries it. The
//! results are the same at any opt-level (no float contraction; rubato's
//! AVX/FMA path is chosen at run time either way).

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Adjustable, Async, FixedAsync, Resampler, SincInterpolationParameters};

/// The sinc stage (module doc).
pub struct SincStage {
    inner: Async<f32>,
    channels: usize,
    frames_in: usize,
}

impl SincStage {
    /// A stage of `channels` interleaved channels that takes exactly
    /// `frames_in` frames per call, at `ratio` (output rate / input rate),
    /// adjustable by up to `max_relative` either way.
    pub fn new(
        ratio: f64,
        max_relative: f64,
        params: &SincInterpolationParameters,
        frames_in: usize,
        channels: usize,
    ) -> Result<Self, String> {
        let inner = Async::<f32>::new_sinc(
            ratio,
            max_relative,
            params,
            frames_in,
            channels,
            FixedAsync::Input,
        )
        .map_err(|e| e.to_string())?;
        Ok(Self {
            inner,
            channels,
            frames_in,
        })
    }

    /// The ratio relative to the nominal one, ramped across the next call
    /// when `ramp`.
    pub fn set_relative(&mut self, relative: f64, ramp: bool) -> Result<(), String> {
        self.inner
            .set_resample_ratio_relative(relative, ramp)
            .map_err(|e| e.to_string())
    }

    /// Resamples `input` (`frames_in` interleaved frames) into `output`
    /// (room for `max_out_frames` frames), returning the frames written.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> Result<usize, String> {
        let frames_out = self.inner.output_frames_max();
        let input = InterleavedSlice::new(input, self.channels, self.frames_in)
            .map_err(|e| e.to_string())?;
        let mut output = InterleavedSlice::new_mut(output, self.channels, frames_out)
            .map_err(|e| e.to_string())?;
        self.inner
            .process_into_buffer(&input, &mut output, None)
            .map(|(_, written)| written)
            .map_err(|e| e.to_string())
    }

    /// The stage's delay, in output frames.
    pub fn delay_frames(&self) -> usize {
        self.inner.output_delay()
    }

    /// The most frames one call can write.
    pub fn max_out_frames(&self) -> usize {
        self.inner.output_frames_max()
    }
}

#[cfg(test)]
mod tests;
