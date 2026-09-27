//! RED scaffold of #215 addendum 3 (design record 5858472395): the fused,
//! row-banded kernel's API — [`Outgoing`], [`mix_nv12_into`], [`mix_bands`] —
//! wired into the `SP-program` sender, but [`mix_nv12_into`] still runs the
//! OLD two-pass path on the calling thread (`FitPlan::apply` into a scratch
//! buffer, then `blend_nv12_into`): the path box run 2 measured at ~56 ms
//! per 2560×1440 boundary. The kernel lands in the next commit.

use super::{FitPlan, Layout, blend_nv12_into};

/// The most row bands (threads) one mixed picture is painted in.
pub const MAX_MIX_BANDS: usize = 6;

/// One band per this many logical processors, so a mix never takes more
/// than a quarter of the box from the paced senders, OBS and Arena.
pub const CPUS_PER_MIX_BAND: usize = 4;

/// How many row bands the `SP-program` sender paints a mixed picture in on a
/// box with `logical_cpus` logical processors: a quarter of them, at least 1,
/// at most [`MAX_MIX_BANDS`] (the 24-thread box: 6).
pub fn mix_bands(logical_cpus: usize) -> usize {
    (logical_cpus / CPUS_PER_MIX_BAND).clamp(1, MAX_MIX_BANDS)
}

/// The outgoing picture of a mixed boundary, as [`mix_nv12_into`] reads it.
#[derive(Clone, Copy, Debug)]
pub enum Outgoing<'a> {
    /// A picture already in the destination layout (the incoming side's, or
    /// the standby black made in it): blended byte for byte.
    Same(Layout, &'a [u8]),
    /// A picture of the plan's source layout, fitted into its destination
    /// (the incoming layout) as it is blended.
    Fitted(&'a FitPlan, &'a [u8]),
}

/// Mix one boundary's pictures into `out` (appended): every byte
/// `blend(fit(from)[p], to[p], weight)`, computed once, in `bands` row bands
/// (at least 1). Bit-identical to `FitPlan::apply` then `blend_nv12_into`
/// (for [`Outgoing::Same`]: to `blend_nv12_into`), and to any other band
/// count. As long as the two-pass result: the destination's bytes (the
/// `Same` picture's, the plan's destination layout's) that `to` also holds.
///
/// Returns how many threads painted it: one per band (a band with no rows
/// paints nothing), fewer only when a helper thread could not start and its
/// band was painted on the calling thread.
pub fn mix_nv12_into(
    from: Outgoing<'_>,
    to: &[u8],
    weight: u32,
    _bands: usize,
    out: &mut Vec<u8>,
) -> usize {
    match from {
        Outgoing::Same(_, picture) => blend_nv12_into(picture, to, weight, out),
        Outgoing::Fitted(plan, src) => {
            let mut fitted = Vec::new();
            plan.apply(src, &mut fitted);
            blend_nv12_into(&fitted, to, weight, out);
        }
    }
    1
}

#[cfg(test)]
#[path = "nv12_mix_tests.rs"]
mod tests;
