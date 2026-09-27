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
//!   size or stride, the outgoing picture is first FITTED into the incoming
//!   layout ([`FitPlan`], [`fit_nv12_into`]: bilinear, aspect kept, centred,
//!   studio-black bars), so the picture dissolves whatever the catalog's
//!   resolutions (#215 addendum A);
//! - a side that is missing on a boundary (its source stalled past the fill
//!   grace) is the standby: the black picture and silence. There is never a
//!   hole.
//!
//! A Cut is a window of zero boundaries: nothing is mixed, so its output is
//! exactly the #209 hard cut.
//!
//! **The cue gate (#215 addendum B2).** A fade does not start on the cut
//! boundary: it waits ([`Cue::Waiting`]) for the incoming source's first
//! LIVE pair (`SubmitJob::live`: a decoder frame with its song audio, never a
//! paced-output fill, a pre-roll black or a paused frozen picture), with the
//! outgoing source kept on program at full level meanwhile. A playlist whose
//! scene comes on program starts a new song, and its first live pair comes
//! ~10 boundaries after the cut (box run 1, #215 comment 5855833871); a fade
//! laid over that wait mixed the outgoing song against silence. The wait is
//! bounded by [`CUE_WAIT_MAX_SLOTS`]; then the fade starts anyway. A Cut
//! never waits.
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

/// The cue gate's bound (`CUE_WAIT_MAX` = 500 ms): a fade waits at most this
/// many boundaries (15 at 30 fps) for the incoming source's first live pair,
/// then starts anyway ([`Cue::Waiting`]).
pub const CUE_WAIT_MAX_SLOTS: u32 = 15;

/// Half a pixel in Q8: a fitted picture samples its source at the destination
/// pixel CENTRES ([`FitPlan`]).
const HALF_PIXEL_Q8: u64 = 128;

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

/// Where a window stands with its cue (#215 cue gate, see the module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cue {
    /// The mix runs from the window's `start_100ns` (a Cut, or the cue opened).
    Open,
    /// The fade waits for the incoming source's first live pair; the outgoing
    /// source stays on program at full level meanwhile. It opens on
    /// `deadline_100ns` at the latest.
    Waiting { deadline_100ns: i64 },
    /// A later cut came while it was still waiting: it never opens, and its
    /// outgoing source stays on program at full level to the window's end.
    Frozen,
}

/// One transition window on the program grid (see the module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    /// The outgoing source; `None` = nothing was on program (a fade up from
    /// the standby).
    pub from: Option<i64>,
    pub to: i64,
    pub kind: TransitionKind,
    /// The cut boundary: `to` owns every boundary from here on, and the
    /// window takes `from`'s pairs from here to `end_100ns`.
    pub cut_100ns: i64,
    /// The first MIXED boundary: the cut boundary, or the boundary the cue
    /// opened on (the cut boundary while the window still waits).
    pub start_100ns: i64,
    /// The window length the mix curve is laid over.
    pub n_slots: u32,
    /// The first boundary after the window: exclusive, and moved earlier when
    /// a later cut starts inside the window ([`Window::truncate`]). While the
    /// cue waits, the latest end the window can still reach.
    pub end_100ns: i64,
    pub cue: Cue,
    start_index: i64,
}

impl Window {
    /// An OPEN window of a cut from `from` to `to` on `start` with `spec`: its
    /// mix runs from `start` (a Cut: zero boundaries).
    pub fn new(from: Option<i64>, to: i64, start_100ns: i64, spec: &TransitionSpec) -> Self {
        let start_index = grid_index_100ns(start_100ns, GENLOCK_GRID_FPS);
        Self {
            from,
            to,
            kind: spec.kind,
            cut_100ns: start_100ns,
            start_100ns,
            n_slots: spec.n_slots,
            end_100ns: grid_boundary_100ns(start_index + i64::from(spec.n_slots), GENLOCK_GRID_FPS),
            cue: Cue::Open,
            start_index,
        }
    }

    /// The window a cut on `cut` opens (#215 cue gate): a Cut is open at once
    /// (zero boundaries); a Fade waits for the incoming source's first live
    /// pair, at most [`CUE_WAIT_MAX_SLOTS`] boundaries, so until its cue opens
    /// it may end as late as the cut + that wait + `n_slots`.
    pub fn cued(from: Option<i64>, to: i64, cut_100ns: i64, spec: &TransitionSpec) -> Self {
        let mut window = Self::new(from, to, cut_100ns, spec);
        if spec.n_slots > 0 {
            let cut_index = window.start_index;
            let at =
                |slots: u32| grid_boundary_100ns(cut_index + i64::from(slots), GENLOCK_GRID_FPS);
            window.cue = Cue::Waiting {
                deadline_100ns: at(CUE_WAIT_MAX_SLOTS),
            };
            window.end_100ns = at(CUE_WAIT_MAX_SLOTS + spec.n_slots);
        }
        window
    }

    /// Whether the window takes `from`'s pair for boundary `b` (from the cut
    /// boundary to the end, the cue's wait included).
    pub fn covers(&self, boundary_100ns: i64) -> bool {
        boundary_100ns >= self.cut_100ns && boundary_100ns < self.end_100ns
    }

    /// The slot `k` of boundary `b` in the mix, `None` outside it (and while
    /// the cue does not run the mix).
    pub fn slot(&self, boundary_100ns: i64) -> Option<u32> {
        if self.cue != Cue::Open
            || boundary_100ns < self.start_100ns
            || boundary_100ns >= self.end_100ns
        {
            return None;
        }
        u32::try_from(grid_index_100ns(boundary_100ns, GENLOCK_GRID_FPS) - self.start_index).ok()
    }

    /// The cue opened on boundary `at`: the mix runs from there over
    /// `n_slots`, never past an end a later cut set. Returns how many
    /// boundaries the window waited (0 = live on the cut boundary).
    pub fn open(&mut self, at_100ns: i64) -> u32 {
        let at_index = grid_index_100ns(at_100ns, GENLOCK_GRID_FPS);
        let waited = at_index - grid_index_100ns(self.cut_100ns, GENLOCK_GRID_FPS);
        self.start_100ns = at_100ns;
        self.start_index = at_index;
        let mix_end = grid_boundary_100ns(at_index + i64::from(self.n_slots), GENLOCK_GRID_FPS);
        self.end_100ns = self.end_100ns.min(mix_end);
        self.cue = Cue::Open;
        u32::try_from(waited).unwrap_or(0)
    }

    /// End the window at `at` (a later cut starts there). A window whose cue
    /// is still waiting never opens (frozen).
    pub fn truncate(&mut self, at_100ns: i64) {
        self.end_100ns = self.end_100ns.min(at_100ns);
        if matches!(self.cue, Cue::Waiting { .. }) {
            self.cue = Cue::Frozen;
        }
    }

    /// How many boundaries the mix covers: `n_slots`, or fewer once a later
    /// cut truncated it; `n_slots` while the cue waits, 0 once frozen.
    pub fn covered(&self) -> u32 {
        match self.cue {
            Cue::Open => {
                u32::try_from(grid_index_100ns(self.end_100ns, GENLOCK_GRID_FPS) - self.start_index)
                    .unwrap_or(0)
            }
            Cue::Waiting { .. } => self.n_slots,
            Cue::Frozen => 0,
        }
    }

    /// How many of the mix's boundaries are at or before `last` (the last
    /// boundary the program committed); 0 while the cue does not run the mix.
    pub fn served(&self, last: Option<i64>) -> u32 {
        if self.cue != Cue::Open {
            return 0;
        }
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

/// A picture's memory layout; two pictures blend byte for byte only when these
/// are equal (otherwise the outgoing one is fitted first, [`FitPlan`]).
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
/// included), so the two blend byte for byte; it is also the canvas a
/// [`FitPlan`] draws the fitted picture on (the bars).
pub fn black_nv12_into(layout: Layout, out: &mut Vec<u8>) {
    let luma = (layout.stride as usize * layout.height as usize).min(layout.len);
    out.extend(std::iter::repeat_n(16u8, luma));
    out.extend(std::iter::repeat_n(128u8, layout.len - luma));
}

/// One bilinear tap along an axis: the two neighbouring source pixels and the
/// Q8 weight of the second.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Tap {
    i0: usize,
    i1: usize,
    w: u32,
}

/// The tap of destination pixel `d` of a `fitted`-pixel run that shows a
/// `src`-pixel source run: the pixel CENTRES line up, so the source position
/// is `(d + ½) · src / fitted − ½` (in Q8, floored), clamped to the source's
/// first and last pixel.
fn tap(d: u32, fitted: u32, src: u32) -> Tap {
    let q = ((2 * u64::from(d) + 1) * u64::from(src) * HALF_PIXEL_Q8 / u64::from(fitted.max(1)))
        .saturating_sub(HALF_PIXEL_Q8);
    let i0 = (q >> 8) as usize;
    let last = src.saturating_sub(1) as usize;
    if i0 >= last {
        return Tap {
            i0: last,
            i1: last,
            w: 0,
        };
    }
    Tap {
        i0,
        i1: i0 + 1,
        w: (q & 0xff) as u32,
    }
}

/// One bilinear sample in Q8: each row `a·(256 − wx) + b·wx`, then the two
/// rows `top·(256 − wy) + bottom·wy`, rounded.
fn bilinear(p00: u8, p01: u8, p10: u8, p11: u8, wx: u32, wy: u32) -> u8 {
    let top = u32::from(p00) * (Q8_ONE - wx) + u32::from(p01) * wx;
    let bottom = u32::from(p10) * (Q8_ONE - wx) + u32::from(p11) * wx;
    ((top * (Q8_ONE - wy) + bottom * wy + (1 << 15)) >> 16) as u8
}

/// `num / den` rounded to the nearest EVEN number (half up), so a fitted
/// picture's chroma samples line up with its luma.
fn even_round(num: u64, den: u64) -> u64 {
    (num + den) / (2 * den) * 2
}

/// Whether `layout` is an NV12 picture a buffer of `len` bytes holds whole:
/// a non-empty size, a stride that fits a row of chroma pairs, and a luma
/// plane + a half-height chroma plane of `stride` bytes per row.
fn nv12_whole(layout: Layout, len: usize) -> bool {
    let (w, h, stride) = (
        layout.width as usize,
        layout.height as usize,
        layout.stride as usize,
    );
    w > 0 && h > 0 && stride >= 2 * w.div_ceil(2) && len >= stride * (h + h.div_ceil(2))
}

/// How the outgoing picture is fitted into the incoming layout (#215
/// addendum A): its aspect kept, scaled until it fills one axis, centred, with
/// studio-black bars (Y 16, UV 128) on the other. Bilinear in Q8 fixed point,
/// the luma plane and the half-resolution chroma plane each on their own grid.
/// The rectangle is even in every coordinate, so each chroma sample covers
/// exactly its 2×2 luma block. Built ONCE per pair of layouts (the column
/// taps are precomputed, the row taps are one per row) and applied on every
/// boundary of the window that needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FitPlan {
    src: Layout,
    dst: Layout,
    /// The fitted picture's rectangle in the destination, in luma pixels.
    x0: usize,
    y0: usize,
    width: u32,
    height: u32,
    /// Per column of the rectangle: the luma tap.
    luma: Vec<Tap>,
    /// Per column of the rectangle's chroma (half its width): the chroma tap.
    chroma: Vec<Tap>,
}

impl FitPlan {
    /// The plan that fits a `src` picture into `dst`.
    pub fn new(src: Layout, dst: Layout) -> Self {
        let (sw, sh) = (u64::from(src.width), u64::from(src.height));
        let (dw, dh) = (u64::from(dst.width), u64::from(dst.height));
        // Each axis at the source's aspect for the OTHER axis in full, capped
        // at the destination: the wider (relatively) side fills its axis, the
        // other keeps the aspect (equal aspects fill both).
        let fw = even_round(sw * dh, sh.max(1)).max(2).min(dw);
        let fh = even_round(sh * dw, sw.max(1)).max(2).min(dh);
        let (width, height) = (fw as u32, fh as u32);
        let luma = (0..width).map(|x| tap(x, width, src.width)).collect();
        let chroma = (0..width / 2)
            .map(|x| tap(x, width / 2, src.width.div_ceil(2)))
            .collect();
        Self {
            src,
            dst,
            x0: ((dw - fw) / 2) as usize & !1,
            y0: ((dh - fh) / 2) as usize & !1,
            width,
            height,
            luma,
            chroma,
        }
    }

    /// Whether this plan fits a `src` picture into `dst` (the sender keeps
    /// one plan per window and rebuilds it for another pair of layouts).
    pub fn fits(&self, src: Layout, dst: Layout) -> bool {
        self.src == src && self.dst == dst
    }

    /// The fitted rectangle `(x0, y0, width, height)` in destination pixels.
    pub fn rect(&self) -> (usize, usize, u32, u32) {
        (self.x0, self.y0, self.width, self.height)
    }

    /// Fit `src` (the plan's source layout) into the destination layout,
    /// appended to `out`: exactly `dst.len` bytes, the bars studio black. A
    /// source (or destination) buffer that is not a whole NV12 picture of its
    /// layout gives the black canvas alone, never a panic.
    pub fn apply(&self, src: &[u8], out: &mut Vec<u8>) {
        let base = out.len();
        black_nv12_into(self.dst, out);
        if !nv12_whole(self.src, src.len()) || !nv12_whole(self.dst, self.dst.len) {
            return;
        }
        let out = &mut out[base..];
        let (ss, ds) = (self.src.stride as usize, self.dst.stride as usize);
        let (sh, dh) = (self.src.height as usize, self.dst.height as usize);
        let width = self.width as usize;
        for y in 0..self.height {
            let row = tap(y, self.height, self.src.height);
            let (r0, r1) = (&src[row.i0 * ss..], &src[row.i1 * ss..]);
            let at = (self.y0 + y as usize) * ds + self.x0;
            for (o, c) in out[at..at + width].iter_mut().zip(&self.luma) {
                *o = bilinear(r0[c.i0], r0[c.i1], r1[c.i0], r1[c.i1], c.w, row.w);
            }
        }
        // The interleaved UV plane: half the rows, pairs of bytes per column.
        let (s_uv, d_uv) = (ss * sh, ds * dh);
        for y in 0..self.height / 2 {
            let row = tap(y, self.height / 2, self.src.height.div_ceil(2));
            let (r0, r1) = (&src[s_uv + row.i0 * ss..], &src[s_uv + row.i1 * ss..]);
            let at = d_uv + (self.y0 / 2 + y as usize) * ds + self.x0;
            for (uv, c) in out[at..at + width].chunks_exact_mut(2).zip(&self.chroma) {
                for (k, o) in uv.iter_mut().enumerate() {
                    let (a, b) = (2 * c.i0 + k, 2 * c.i1 + k);
                    *o = bilinear(r0[a], r0[b], r1[a], r1[b], c.w, row.w);
                }
            }
        }
    }
}

/// Fit the NV12 picture `src` of `src_layout` into `dst_layout` (appended to
/// `out`, [`FitPlan`]). The program sender keeps its plan across a window's
/// boundaries; this one-shot form builds it every call.
pub fn fit_nv12_into(src: &[u8], src_layout: Layout, dst_layout: Layout, out: &mut Vec<u8>) {
    FitPlan::new(src_layout, dst_layout).apply(src, out);
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
    /// #215 cue gate: how many boundaries the LAST fade waited for the
    /// incoming source's first live pair (0 = live on the cut boundary,
    /// [`CUE_WAIT_MAX_SLOTS`] = it timed out).
    pub cue_wait_boundaries: u64,
    /// Fades that started without a live incoming pair once the cue gate's
    /// wait ran out.
    pub cue_timeouts: u64,
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
