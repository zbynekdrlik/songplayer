//! Scene transitions on `SP-program` (#215, B5 of EPIC #174): the PURE layer.
//!
//! A program cut is a transition WINDOW on the genlock grid (design record
//! #215 comment 5853036223, Approach 1). After a cut from `from` to `to` on
//! boundary `start`, `to` owns every boundary from `start` on, as before (#209),
//! and `from` ALSO contributes to the `n` boundaries `[start, end)`. On each of
//! them the program bus assembles both sources' pairs and the `SP-program`
//! sender emits ONE mixed pair:
//!
//! - **audio**: an equal-power crossfade, `a = a_from·cos θ + a_to·sin θ`, with
//!   θ running 0 → π/2 continuously over all `n × 1600` samples of the window
//!   ([`crossfade_gains`]), so the gain never steps at a boundary edge;
//! - **video**: a per-pixel linear blend of the two NV12 frames at the
//!   boundary's midpoint fraction α = (k + ½)/n, in integer Q8 math
//!   ([`weight_q8`], [`blend_nv12_into`]). When the two pictures differ in
//!   size or stride, the picture CUTS at the window's midpoint instead
//!   ([`picture_mix`]) while the audio still crossfades;
//! - a side that is missing on a boundary (its source stalled past the fill
//!   grace) is the standby: the black picture and silence. There is never a
//!   hole.
//!
//! A Cut is a window of zero boundaries: nothing is mixed, so its output is
//! exactly the #209 hard cut.
//!
//! The window's stamps use exact grid indices
//! (`sp_core::genlock::{grid_index_100ns, grid_boundary_100ns}`): at 30 fps
//! the slots are 333 333 or 333 334 × 100 ns wide, so `start + k · interval`
//! would drift off the grid.
//!
//! This file also holds the transition SPEC, meaning what the next cut does:
//! cg OBS's current transition or the operator's override
//! ([`effective_spec`]), plus the telemetry types. The window bookkeeping
//! lives in `program_bus.rs`, the mixing call in `program_output.rs`, and the
//! OBS follow task in `program_follow.rs`.

use std::f64::consts::FRAC_PI_2;

use serde::Serialize;
use sp_core::genlock::{GENLOCK_GRID_FPS, grid_boundary_100ns, grid_index_100ns};
use sp_ndi::AudioFrame;

use crate::playback::submit_handoff::SubmitJob;

/// The longest window a transition may take: 300 slots (10 s at 30 fps). A
/// longer OBS or configured duration is clamped to it.
pub const MAX_TRANSITION_SLOTS: u32 = 300;

/// The Q8 weight of the `to` picture: 0 = all `from`, 256 = all `to`.
pub const Q8_ONE: u32 = 256;

/// What a cut does on `SP-program`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TransitionKind {
    /// A hard switch on one boundary (the #209 cut).
    Cut,
    /// A crossfade over `n_slots` boundaries.
    Fade,
}

/// Where the transition in force comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SpecSource {
    /// cg OBS's current scene transition (`program_transition = obs`).
    Obs,
    /// The operator's override (`program_transition = fade | cut`).
    Setting,
    /// `obs`, but cg OBS's transition is not known yet: a Fade of
    /// `program_transition_ms`.
    Fallback,
}

/// The transition the next cut uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct TransitionSpec {
    pub kind: TransitionKind,
    /// The configured or OBS duration (0 for a Cut).
    pub duration_ms: u32,
    /// The window length in grid slots (0 for a Cut).
    pub n_slots: u32,
    pub source: SpecSource,
}

impl TransitionSpec {
    /// A hard cut.
    pub fn cut(source: SpecSource) -> Self {
        Self {
            kind: TransitionKind::Cut,
            duration_ms: 0,
            n_slots: 0,
            source,
        }
    }

    /// A crossfade of `duration_ms`, rounded to whole slots ([`slots_for_ms`]).
    pub fn fade(duration_ms: u32, source: SpecSource) -> Self {
        Self {
            kind: TransitionKind::Fade,
            duration_ms,
            n_slots: slots_for_ms(duration_ms),
            source,
        }
    }
}

/// A duration in whole grid slots: rounded to the nearest slot, at least 1,
/// at most [`MAX_TRANSITION_SLOTS`] (300 ms → 9 slots at 30 fps).
pub fn slots_for_ms(duration_ms: u32) -> u32 {
    let slots = (u64::from(duration_ms) * GENLOCK_GRID_FPS as u64 + 500) / 1000;
    slots.clamp(1, u64::from(MAX_TRANSITION_SLOTS)) as u32
}

/// The operator's `program_transition` setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TransitionMode {
    /// Follow cg OBS's current scene transition (the default).
    Obs,
    /// Always a Fade of `program_transition_ms`.
    Fade,
    /// Always a hard Cut.
    Cut,
}

impl TransitionMode {
    /// `fade` / `cut` (trimmed); anything else, or no value, is `obs`.
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim) {
            Some("fade") => Self::Fade,
            Some("cut") => Self::Cut,
            _ => Self::Obs,
        }
    }
}

/// cg OBS's current scene transition, as `GetCurrentSceneTransition` reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ObsTransition {
    /// The transition's name in cg OBS (e.g. `Fade`).
    pub name: String,
    /// Its kind (`fade_transition`, `cut_transition`, `swipe_transition`, …).
    pub kind: String,
    /// Its duration; `None` for a fixed-duration transition.
    pub duration_ms: Option<u32>,
}

/// cg OBS's transition as a spec: `cut_transition` → Cut; every other kind
/// (fade, swipe, stinger, …) → a Fade of its duration, or of `fallback_ms`
/// when it has none.
pub fn spec_from_obs(obs: &ObsTransition, fallback_ms: u32) -> TransitionSpec {
    if obs.kind == "cut_transition" {
        return TransitionSpec::cut(SpecSource::Obs);
    }
    TransitionSpec::fade(obs.duration_ms.unwrap_or(fallback_ms), SpecSource::Obs)
}

/// The spec every cut (dashboard, #213 remote, OBS follow) uses: the
/// operator's override, else cg OBS's transition, else a Fade of `ms`.
pub fn effective_spec(
    mode: TransitionMode,
    ms: u32,
    obs: Option<&ObsTransition>,
) -> TransitionSpec {
    match (mode, obs) {
        (TransitionMode::Cut, _) => TransitionSpec::cut(SpecSource::Setting),
        (TransitionMode::Fade, _) => TransitionSpec::fade(ms, SpecSource::Setting),
        (TransitionMode::Obs, Some(obs)) => spec_from_obs(obs, ms),
        (TransitionMode::Obs, None) => TransitionSpec::fade(ms, SpecSource::Fallback),
    }
}

/// One transition window on the program grid (see the module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    /// The outgoing source; `None` = nothing was on program (a fade up from
    /// the standby).
    pub from: Option<i64>,
    pub to: i64,
    pub kind: TransitionKind,
    /// The first boundary `to` owns (the cut boundary).
    pub start_100ns: i64,
    /// The window length the mix curve is laid over.
    pub n_slots: u32,
    /// The first boundary after the window: exclusive, and moved earlier when
    /// a later cut starts inside the window ([`Window::truncate`]).
    pub end_100ns: i64,
    start_index: i64,
}

impl Window {
    /// The window of a cut from `from` to `to` on `start` with `spec`.
    pub fn new(from: Option<i64>, to: i64, start_100ns: i64, spec: &TransitionSpec) -> Self {
        let start_index = grid_index_100ns(start_100ns, GENLOCK_GRID_FPS);
        Self {
            from,
            to,
            kind: spec.kind,
            start_100ns,
            n_slots: spec.n_slots,
            end_100ns: grid_boundary_100ns(start_index + i64::from(spec.n_slots), GENLOCK_GRID_FPS),
            start_index,
        }
    }

    /// The slot `k` of boundary `b` inside the window, `None` outside it.
    pub fn slot(&self, boundary_100ns: i64) -> Option<u32> {
        if boundary_100ns < self.start_100ns || boundary_100ns >= self.end_100ns {
            return None;
        }
        u32::try_from(grid_index_100ns(boundary_100ns, GENLOCK_GRID_FPS) - self.start_index).ok()
    }

    /// End the window at `at` (a later cut starts there).
    pub fn truncate(&mut self, at_100ns: i64) {
        self.end_100ns = self.end_100ns.min(at_100ns);
    }

    /// How many boundaries the window covers: `n_slots`, or fewer once a
    /// later cut truncated it.
    pub fn covered(&self) -> u32 {
        u32::try_from(grid_index_100ns(self.end_100ns, GENLOCK_GRID_FPS) - self.start_index)
            .unwrap_or(0)
    }

    /// How many of the window's boundaries are at or before `last` (the last
    /// boundary the program committed).
    pub fn served(&self, last: Option<i64>) -> u32 {
        let upto = last.map_or(0, |l| {
            grid_index_100ns(l, GENLOCK_GRID_FPS) - self.start_index + 1
        });
        u32::try_from(upto.clamp(0, i64::from(self.covered()))).unwrap_or(0)
    }
}

/// The Q8 weight of the `to` picture on slot `k` of an `n`-slot window: the
/// midpoint fraction (k + ½)/n, rounded (`k = 4` of `n = 9` → 128 = ½).
pub fn weight_q8(slot: u32, n_slots: u32) -> u32 {
    let n = u64::from(n_slots.max(1));
    let w = (u64::from(Q8_ONE) * (2 * u64::from(slot) + 1) + n) / (2 * n);
    w.min(u64::from(Q8_ONE)) as u32
}

/// Blend two NV12 frames of the SAME layout into `out` (appended): per byte
/// `(f·(256 − w) + t·w + 128) >> 8`. Y and the interleaved UV plane blend
/// alike, since both are linear in the same weight.
pub fn blend_nv12_into(from: &[u8], to: &[u8], weight: u32, out: &mut Vec<u8>) {
    let weight = weight.min(Q8_ONE);
    let keep = Q8_ONE - weight;
    out.extend(
        from.iter()
            .zip(to)
            .map(|(&f, &t)| ((u32::from(f) * keep + u32::from(t) * weight + 128) >> 8) as u8),
    );
}

/// A picture's memory layout; two pictures blend only when these are equal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub len: usize,
}

impl Layout {
    /// The layout of a source's boundary picture.
    pub fn of(job: &SubmitJob) -> Self {
        Self {
            width: job.width,
            height: job.height,
            stride: job.stride,
            len: job.video.len(),
        }
    }
}

/// The NV12 studio black of `layout`, appended to `out`: Y 16 over the
/// `stride × height` luma plane, then 128 (neutral chroma) for the rest of the
/// buffer. A missing side of a mixed boundary is this black in the PRESENT
/// side's exact layout (a decoder's stride padding and buffer length
/// included), so the two always blend and never cut at the midpoint.
pub fn black_nv12_into(layout: Layout, out: &mut Vec<u8>) {
    let luma = (layout.stride as usize * layout.height as usize).min(layout.len);
    out.extend(std::iter::repeat_n(16u8, luma));
    out.extend(std::iter::repeat_n(128u8, layout.len - luma));
}

/// How a mixed boundary builds its picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Picture {
    /// Blend both pictures (same layout).
    Blend,
    /// Different layouts, first half of the window: the `from` picture.
    From,
    /// Different layouts, from the midpoint on: the `to` picture.
    To,
}

/// Blend two pictures of the same layout; otherwise cut the picture at the
/// window's midpoint (`weight ≥ ½`).
pub fn picture_mix(from: Layout, to: Layout, weight: u32) -> Picture {
    if from == to {
        Picture::Blend
    } else if weight >= Q8_ONE / 2 {
        Picture::To
    } else {
        Picture::From
    }
}

/// Whether a mixed boundary's picture decision starts a run of midpoint cuts,
/// so the sender logs the size mismatch once per window, not per boundary.
pub fn starts_size_cut(previous_was_cut: bool, picture: Picture) -> bool {
    picture != Picture::Blend && !previous_was_cut
}

/// The equal-power gains `(cos θ, sin θ)` of sample `j` of a `total`-sample
/// window, θ = π/2 · (j + ½)/total. θ steps by exactly π/(2·total) per sample,
/// across boundary edges too.
pub fn crossfade_gains(sample: u64, total: u64) -> (f32, f32) {
    let theta = FRAC_PI_2 * (sample as f64 + 0.5) / total.max(1) as f64;
    (theta.cos() as f32, theta.sin() as f32)
}

/// Sample `channel` of frame `frame` of a side's block: a mono block feeds
/// every channel, and a missing side, sample or channel is silence.
fn side_sample(side: Option<&AudioFrame>, frame: usize, channel: usize) -> f32 {
    let Some(block) = side else {
        return 0.0;
    };
    let channels = block.channels as usize;
    if channels == 0 {
        return 0.0;
    }
    block
        .data
        .get(frame * channels + channel.min(channels - 1))
        .copied()
        .unwrap_or(0.0)
}

/// The program's audio format for one mixed block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioFormat {
    /// Frames per boundary (1600 at 48 kHz / 30 fps).
    pub frames: usize,
    pub channels: u32,
    pub sample_rate: u32,
}

/// Crossfade one boundary's blocks: frame `i` is sample `first + i` of a
/// `total`-sample window.
pub fn mix_audio_block(
    from: Option<&AudioFrame>,
    to: Option<&AudioFrame>,
    first: u64,
    total: u64,
    format: AudioFormat,
) -> AudioFrame {
    let channels = format.channels as usize;
    // Collected, not pre-sized: a capacity formula would be an equivalent
    // mutant (only the allocation changes).
    let data = (0..format.frames)
        .flat_map(|i| {
            let (g_from, g_to) = crossfade_gains(first + i as u64, total);
            (0..channels)
                .map(move |c| g_from * side_sample(from, i, c) + g_to * side_sample(to, i, c))
        })
        .collect();
    AudioFrame {
        data,
        channels: format.channels,
        sample_rate: format.sample_rate,
        timecode_100ns: None,
    }
}

/// One boundary inside a transition window, for the `SP-program` sender: both
/// sources' pairs (`None` = that side is the standby).
pub struct MixJob {
    pub stamp_100ns: i64,
    pub from: Option<SubmitJob>,
    pub to: Option<SubmitJob>,
    /// The boundary's slot `k` in the window.
    pub slot: u32,
    pub n_slots: u32,
}

impl MixJob {
    /// The `to` picture's Q8 weight on this boundary.
    pub fn weight_q8(&self) -> u32 {
        weight_q8(self.slot, self.n_slots)
    }

    /// The window sample index of this boundary's first frame, and the
    /// window's total samples, for `frames` frames per boundary.
    pub fn sample_span(&self, frames: usize) -> (u64, u64) {
        let frames = frames as u64;
        (
            u64::from(self.slot) * frames,
            u64::from(self.n_slots) * frames,
        )
    }
}

/// Transition counters (`GET /api/v1/program` → `transition`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TransitionCounters {
    /// Windows the program served to their end (cuts included).
    pub transitions_done: u64,
    /// Boundaries emitted as one mixed pair.
    pub mixed_boundaries: u64,
    /// Mixed boundaries on which a source's side was missing (mixed against
    /// the standby).
    pub side_fills: u64,
}

/// The running or next transition window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ActiveWindow {
    pub from: Option<i64>,
    pub to: i64,
    pub start_boundary_100ns: i64,
    pub n_slots: u32,
    /// Window boundaries the program already emitted.
    pub served_slots: u32,
    /// `served_slots` in percent of the boundaries the window covers
    /// (`n_slots`, or fewer once a later cut truncated it).
    pub progress: u32,
}

impl ActiveWindow {
    /// `w` as served up to `last`.
    pub fn of(w: &Window, last: Option<i64>) -> Self {
        let served = w.served(last);
        Self {
            from: w.from,
            to: w.to,
            start_boundary_100ns: w.start_100ns,
            n_slots: w.n_slots,
            served_slots: served,
            progress: served * 100 / w.covered().max(1),
        }
    }
}

/// `GET /api/v1/program` → `transition`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct TransitionStatus {
    /// The spec the next cut uses.
    pub kind: TransitionKind,
    pub duration_ms: u32,
    pub n_slots: u32,
    pub source: SpecSource,
    /// The running (or pending) fade window, `None` when none.
    pub active: Option<ActiveWindow>,
    #[serde(flatten)]
    pub counters: TransitionCounters,
}

#[cfg(test)]
#[path = "program_transition_tests.rs"]
mod tests;
