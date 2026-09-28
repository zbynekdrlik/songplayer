//! #215 addendum 3: the fused, row-banded NV12 mix (`nv12_mix.rs`). It must
//! equal the two-pass reference bit for bit (`FitPlan::apply` then
//! `blend_nv12_into`; `blend_nv12_into` alone for one layout) on random
//! frames, at every weight, for every band count, whatever the row count, and
//! for any split of the picture into runs. Every band beyond the first runs
//! on a helper thread of its own. The frames come from a fixed-seed
//! SplitMix64, so every run tests the same bytes.
//! Wired via `#[cfg(test)] #[path = "nv12_mix_tests.rs"] mod tests;`.

use super::*;
use crate::playback::program_transition::{FitPlan, Layout, blend_nv12_into};

/// A deterministic byte source (SplitMix64).
struct Bytes(u64);

impl Bytes {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn frame(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }
}

/// A `width`×`height` NV12 layout with `stride`-byte rows: the luma plane,
/// then the half-height chroma plane.
const fn nv12(width: u32, height: u32, stride: u32) -> Layout {
    Layout {
        width,
        height,
        stride,
        len: stride as usize * (height as usize + height.div_ceil(2) as usize),
    }
}

/// The weights every equality test runs: both ends, the half, just inside
/// both ends, past all-`to`, and two others.
const WEIGHTS: [u32; 8] = [0, 1, 128, 255, 256, 300, 37, 201];

/// The two-pass reference: the fit into a scratch buffer, then the blend.
fn reference(from: Outgoing<'_>, to: &[u8], weight: u32) -> Vec<u8> {
    let mut out = Vec::new();
    match from {
        Outgoing::Same(_, picture) => blend_nv12_into(picture, to, weight, &mut out),
        Outgoing::Fitted(plan, src) => {
            let mut fitted = Vec::new();
            plan.apply(src, &mut fitted);
            blend_nv12_into(&fitted, to, weight, &mut out);
        }
    }
    out
}

/// The fused mix in `bands` bands, checking that it appends.
fn mixed(from: Outgoing<'_>, to: &[u8], weight: u32, bands: usize) -> Vec<u8> {
    let mut out = vec![9u8];
    mix_nv12_into(from, to, weight, bands, &mut out);
    assert_eq!(
        out.remove(0),
        9,
        "the mix appends after what the buffer holds"
    );
    out
}

/// `got` is `want` byte for byte (naming the first difference, not two dumps).
fn assert_same(got: &[u8], want: &[u8], what: &str) {
    assert_eq!(got.len(), want.len(), "{what}: length");
    if let Some(p) = got.iter().zip(want).position(|(g, w)| g != w) {
        panic!("{what}: byte {p} is {} instead of {}", got[p], want[p]);
    }
}

#[test]
fn the_sender_paints_in_a_quarter_of_the_logical_processors_at_most_six_bands() {
    assert_eq!((MAX_MIX_BANDS, CPUS_PER_MIX_BAND), (6, 4));
    for (cpus, bands) in [
        (0, 1),
        (1, 1),
        (3, 1),
        (4, 1),
        (7, 1),
        (8, 2),
        (12, 3),
        (23, 5),
        (24, 6),
        (25, 6),
        (64, 6),
    ] {
        assert_eq!(mix_bands(cpus), bands, "{cpus} logical processors");
    }
}

#[test]
fn the_fused_fit_and_blend_equal_the_fit_then_the_blend_on_random_frames() {
    // The design's pair (64×36 ↔ 48×40), the catalog's resolutions at a
    // tenth (21:9 into 16:9 and back, 1080p into 1440p, 21:9 into 1080p), and
    // decoder-padded strides on both sides.
    let pairs = [
        (nv12(64, 36, 64), nv12(48, 40, 48)),
        (nv12(48, 40, 48), nv12(64, 36, 64)),
        (nv12(256, 108, 256), nv12(256, 144, 256)),
        (nv12(256, 144, 256), nv12(256, 108, 256)),
        (nv12(192, 108, 192), nv12(256, 144, 256)),
        (nv12(256, 108, 256), nv12(192, 108, 192)),
        (nv12(64, 36, 72), nv12(48, 40, 56)),
    ];
    let mut bytes = Bytes(215);
    for (src_layout, dst_layout) in pairs {
        let src = bytes.frame(src_layout.len);
        let to = bytes.frame(dst_layout.len);
        let plan = FitPlan::new(src_layout, dst_layout);
        let random = [(bytes.next() % 257) as u32, (bytes.next() % 257) as u32];
        for weight in WEIGHTS.into_iter().chain(random) {
            let from = Outgoing::Fitted(&plan, &src);
            let want = reference(from, &to, weight);
            assert_eq!(want.len(), dst_layout.len);
            for bands in 1..=MAX_MIX_BANDS {
                assert_same(
                    &mixed(from, &to, weight, bands),
                    &want,
                    &format!("{src_layout:?} into {dst_layout:?}, weight {weight}, {bands} bands"),
                );
            }
        }
    }
}

#[test]
fn the_equal_layout_mix_equals_the_blend_on_random_frames() {
    let mut bytes = Bytes(5_858_472_395);
    for layout in [nv12(64, 36, 64), nv12(64, 36, 72), nv12(48, 40, 48)] {
        let from = bytes.frame(layout.len);
        let to = bytes.frame(layout.len);
        for weight in WEIGHTS {
            let same = Outgoing::Same(layout, &from);
            let want = reference(same, &to, weight);
            for bands in 1..=MAX_MIX_BANDS {
                assert_same(
                    &mixed(same, &to, weight, bands),
                    &want,
                    &format!("{layout:?}, weight {weight}, {bands} bands"),
                );
            }
        }
    }
}

#[test]
fn every_band_count_paints_what_one_band_paints_whatever_the_row_count() {
    // Row counts that 2..6 do not divide, and pictures of fewer rows than
    // bands (a band may get no rows at all).
    let mut bytes = Bytes(33);
    let src_layout = nv12(9, 7, 10);
    let src = bytes.frame(src_layout.len);
    for height in [1, 2, 3, 5, 7, 11] {
        let dst_layout = nv12(6, height, 8);
        let to = bytes.frame(dst_layout.len);
        let plan = FitPlan::new(src_layout, dst_layout);
        let same_picture = bytes.frame(dst_layout.len);
        for from in [
            Outgoing::Fitted(&plan, &src),
            Outgoing::Same(dst_layout, &same_picture),
        ] {
            let one = mixed(from, &to, 90, 1);
            assert_same(&one, &reference(from, &to, 90), "one band");
            for bands in 2..=MAX_MIX_BANDS {
                assert_same(
                    &mixed(from, &to, 90, bands),
                    &one,
                    &format!("{height} rows in {bands} bands"),
                );
            }
        }
    }
}

/// Small layouts, whole and not: tight, padded, odd, under 2×2, a zero
/// stride (a zero-width picture), a buffer too short for its layout, a
/// stride too short for a row, and bytes past the chroma plane.
const SWEEP: [Layout; 13] = [
    nv12(4, 2, 4),
    nv12(4, 2, 6),
    nv12(6, 2, 6),
    nv12(5, 3, 6),
    nv12(8, 4, 8),
    nv12(1, 1, 2),
    nv12(2, 5, 2),
    nv12(7, 5, 8),
    Layout {
        width: 0,
        height: 2,
        stride: 0,
        len: 0,
    },
    Layout {
        width: 0,
        height: 3,
        stride: 0,
        len: 4,
    },
    Layout {
        width: 4,
        height: 4,
        stride: 4,
        len: 20,
    },
    Layout {
        width: 4,
        height: 2,
        stride: 3,
        len: 9,
    },
    Layout {
        width: 4,
        height: 2,
        stride: 4,
        len: 15,
    },
];

#[test]
fn every_small_or_broken_layout_mixes_like_the_two_pass_reference() {
    // Every pair of the sweep, with a whole or a one-byte-short source and an
    // incoming picture of its layout's length or one byte short (the output
    // is then as long as the reference's: the bytes both sides hold).
    let mut bytes = Bytes(1_000);
    for src_layout in SWEEP {
        for dst_layout in SWEEP {
            let plan = FitPlan::new(src_layout, dst_layout);
            let whole_src = bytes.frame(src_layout.len);
            let whole_to = bytes.frame(dst_layout.len);
            let short = |v: &[u8]| v[..v.len().saturating_sub(1)].to_vec();
            for src in [whole_src.clone(), short(&whole_src)] {
                for to in [whole_to.clone(), short(&whole_to)] {
                    let mut sides = vec![("fitted", Outgoing::Fitted(&plan, &src))];
                    if src_layout == dst_layout {
                        sides.push(("same", Outgoing::Same(dst_layout, &src)));
                    }
                    for (kind, from) in sides {
                        for weight in [0, 100, 256] {
                            let want = reference(from, &to, weight);
                            for bands in [1, 3] {
                                assert_same(
                                    &mixed(from, &to, weight, bands),
                                    &want,
                                    &format!(
                                        "{kind} {src_layout:?} ({} bytes) into {dst_layout:?} \
                                         ({} bytes), weight {weight}, {bands} bands",
                                        src.len(),
                                        to.len()
                                    ),
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn every_band_beyond_the_first_is_painted_on_a_helper_thread() {
    // The fix itself (box run 2: the fit and the blend ran on the one
    // `SP-program` thread): a picture in K bands is painted by K threads,
    // the calling one and one helper per further band, even when some bands
    // get no rows.
    let mut bytes = Bytes(7);
    let (src_layout, dst_layout) = (nv12(64, 36, 64), nv12(48, 40, 48));
    let (src, to) = (bytes.frame(src_layout.len), bytes.frame(dst_layout.len));
    let plan = FitPlan::new(src_layout, dst_layout);
    let same = bytes.frame(dst_layout.len);
    let flat = nv12(6, 2, 6);
    let flat_picture = bytes.frame(flat.len);
    for bands in 1..=MAX_MIX_BANDS {
        let mut out = Vec::new();
        assert_eq!(
            mix_nv12_into(Outgoing::Fitted(&plan, &src), &to, 128, bands, &mut out),
            bands,
            "the fitted mix in {bands} bands"
        );
        assert_eq!(
            mix_nv12_into(Outgoing::Same(dst_layout, &same), &to, 128, bands, &mut out),
            bands,
            "the equal-layout mix in {bands} bands"
        );
        assert_eq!(
            mix_nv12_into(
                Outgoing::Same(flat, &flat_picture),
                &flat_picture,
                128,
                bands,
                &mut out
            ),
            bands,
            "a 2-row picture in {bands} bands"
        );
    }
}

#[test]
fn a_run_may_start_anywhere_in_a_row() {
    // The bands start on row edges, but the painter must not rely on it:
    // cutting the destination anywhere (mid-row, inside the bars, inside
    // the picture, on the plane edge) paints the same bytes.
    let mut bytes = Bytes(99);
    let (src_layout, dst_layout) = (nv12(10, 4, 12), nv12(16, 6, 18));
    let (src, to) = (bytes.frame(src_layout.len), bytes.frame(dst_layout.len));
    let plan = FitPlan::new(src_layout, dst_layout);
    assert_eq!(plan.rect(), (0, 0, 14, 6), "a pillarbox: bars right of it");
    let same = bytes.frame(dst_layout.len);
    for (kind, from) in [
        ("fitted", Outgoing::Fitted(&plan, &src)),
        ("same", Outgoing::Same(dst_layout, &same)),
    ] {
        let want = reference(from, &to, 77);
        for cuts in [
            vec![5, 23, 40, 107, 108, 115, 130],
            vec![1, 2, 3, 17, 19, 33, 35, 150, 161],
            vec![13, 14, 15, 16, 17, 100, 120, 145],
        ] {
            let mix = Mix {
                from,
                to: &to,
                weight: Weight::new(77),
            };
            let mut got = vec![0u8; dst_layout.len];
            let mut at = 0;
            for end in cuts.into_iter().chain([dst_layout.len]) {
                mix.paint(at, &mut got[at..end]);
                at = end;
            }
            assert_same(&got, &want, &format!("the {kind} mix cut into runs"));
        }
    }
}

#[test]
fn the_rows_are_shared_out_evenly_across_the_bands() {
    // Band i: luma rows dh·i/k .. dh·(i+1)/k, chroma rows ch·i/k ..
    // ch·(i+1)/k; the last band also every byte past the chroma plane; every
    // offset capped at the bytes mixed.
    let full = nv12(2560, 1440, 2560);
    let s = 2560;
    assert_eq!(
        band_bounds(full, full.len, 6),
        vec![
            0,
            240 * s,
            480 * s,
            720 * s,
            960 * s,
            1200 * s,
            1440 * s,
            1560 * s,
            1680 * s,
            1800 * s,
            1920 * s,
            2040 * s,
            full.len,
        ],
        "1440 luma rows, 720 chroma rows, six bands"
    );
    assert_eq!(
        band_bounds(nv12(4, 7, 4), 50, 3),
        vec![0, 8, 16, 28, 32, 36, 50],
        "7 luma rows as 2 + 2 + 3, 4 chroma rows as 1 + 1 + 2, then the bytes past them"
    );
    assert_eq!(
        band_bounds(nv12(6, 2, 6), 18, 4),
        vec![0, 0, 6, 6, 12, 12, 12, 12, 18],
        "2 rows in 4 bands: bands 1 and 3 get a luma row, band 3 the chroma row"
    );
    assert_eq!(
        band_bounds(nv12(6, 2, 6), 10, 2),
        vec![0, 6, 10, 10, 10],
        "capped at the 10 bytes both pictures hold"
    );
    assert_eq!(
        band_bounds(nv12(6, 2, 6), 18, 1),
        vec![0, 12, 18],
        "one band: the luma plane, then the rest"
    );
}
