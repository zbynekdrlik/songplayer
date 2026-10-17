//! #251: frame blending for a source the grid does not divide.
//!
//! A 25 fps song on the 30 fps grid used to show the newest frame due at
//! each boundary, so every 5th boundary repeated a frame (cadence
//! 1-1-1-1-2); the 60 Hz wall then held the song's pictures 2-2-2-2-4
//! refreshes and every pan hitched five times a second (PP, 11.10.2026,
//! owner's ROZHODNUTÉ on #251: blend). Now a boundary that falls BETWEEN
//! two source frames shows both: the frame due at it, blended toward the
//! next decoded one by how far the boundary lies between them
//! ([`blend_weight_q8`]). The pacer only decides it; the `SP-program`
//! sender paints the blend in its canvas (one pass, `Canvas::fade`) and
//! `SP-program-MAX` draws it on the GPU (two quads), exactly as a scene
//! fade's two sources.
//!
//! Only a source SLOWER than the grid and faster than half of it (20–25
//! fps on 30) is blended: its gap between frames lies between one slot and
//! two, each [`BLEND_DEAD_BAND_100NS`] inside. A 29.97 / 30 / 50 / 59.94 fps
//! source keeps its sharp frames (blending one drifts through every weight,
//! review of #251: 95 % of a 29.97 fps song's boundaries); a 15 fps one keeps
//! its even 2-slot hold; a stall or a seek is never bridged; a next frame of
//! another size is not blended.

use sp_ndi::AudioFrame;

use crate::playback::pacer::{PacedFrame, PacedSink, Pacer};
use crate::playback::picture::Picture;

/// A boundary less than this (100 ns, 2 ms) after the shown frame's own
/// present time shows that frame alone: the millisecond pts of a source
/// the grid divides (30 fps) lands up to 1 ms before its boundary.
pub const BLEND_DEAD_BAND_100NS: i64 = 20_000;

/// The next decoded frame a boundary's picture is blended toward, and its
/// Q8 weight (1..=255): the incoming side of `Canvas::fade` and of
/// `SP-program-MAX`'s fade. The next frame has the shown frame's size and
/// stride (the pacer checks it).
#[derive(Clone, Debug)]
pub struct FrameBlend {
    pub video: Picture,
    pub weight_q8: u32,
}

/// The Q8 weight of the next frame at a boundary `into_100ns` after the
/// shown frame's present time, the next frame presenting `gap_100ns` after
/// it, on a grid of `interval_100ns`: `into · 256 / gap`, floored. `None`
/// (show the frame alone) inside the dead band, at or past the next frame,
/// or for a gap not strictly inside (one slot, two slots) by the dead band.
pub fn blend_weight_q8(into_100ns: i64, gap_100ns: i64, interval_100ns: i64) -> Option<u32> {
    if into_100ns < BLEND_DEAD_BAND_100NS
        || into_100ns >= gap_100ns
        || gap_100ns <= interval_100ns + BLEND_DEAD_BAND_100NS
        || gap_100ns >= 2 * interval_100ns - BLEND_DEAD_BAND_100NS
    {
        return None;
    }
    u32::try_from(into_100ns * 256 / gap_100ns).ok()
}

impl Pacer {
    /// Emit `shown` on the boundary stamped `stamp_100ns` — blended toward
    /// the next decoded frame (the parked `pending`) when the serviced
    /// `boundary_100ns` lies between the two ([`blend_weight_q8`]), else as
    /// it is.
    pub(super) fn emit_shown<S: PacedSink>(
        &self,
        sink: &mut S,
        shown: &PacedFrame,
        audio: &[AudioFrame],
        boundary_100ns: i64,
        stamp_100ns: i64,
    ) {
        match self.blend_toward_next(shown, boundary_100ns) {
            Some(blend) => sink.emit_blended(shown, blend, audio, stamp_100ns, stamp_100ns),
            None => sink.emit(shown, audio, stamp_100ns, stamp_100ns),
        }
    }

    /// The blend of `shown` toward the parked next frame at `boundary_100ns`,
    /// or `None`: no next frame, a next frame of another size or stride, or
    /// no weight ([`blend_weight_q8`]).
    fn blend_toward_next(&self, shown: &PacedFrame, boundary_100ns: i64) -> Option<FrameBlend> {
        let next = self.pending.as_ref()?;
        if (next.width, next.height, next.stride) != (shown.width, shown.height, shown.stride) {
            return None;
        }
        let present = |f: &PacedFrame| self.wall_start_100ns.saturating_add(f.pts_100ns());
        let weight_q8 = blend_weight_q8(
            boundary_100ns - present(shown),
            present(next) - present(shown),
            self.interval_100ns,
        )?;
        Some(FrameBlend {
            video: next.video.clone(),
            weight_q8,
        })
    }
}

#[cfg(test)]
#[path = "pacer_blend_tests.rs"]
mod tests;
