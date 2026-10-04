//! #215 addendum 3 + #223 follow-up: the fused, row-banded NV12 paint
//! (`nv12_mix.rs`). It must equal the two-pass reference bit for bit — each
//! side made a whole destination picture first (`FitPlan::apply`, its own
//! bytes, or `black_nv12_into`), then `blend_nv12_into` — on random frames,
//! at every weight, for every band count, whatever the row count, and for any
//! split of the picture into runs. A fade reads BOTH sides in its one pass
//! (each fitted, as it is, or the black); a plain fit reads its one side.
//! Every band beyond the first runs on a worker of the pool. The frames come
//! from a fixed-seed SplitMix64, so every run tests the same bytes; the exact
//! pins come from a scratch Python model of the kernel.
//! Wired via `#[cfg(test)] #[path = "nv12_mix_tests.rs"] mod tests;`.

use super::*;
use crate::playback::band_pool::BandPool;
use crate::playback::program_transition::{FitPlan, Layout, black_nv12_into, blend_nv12_into};

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

/// One pool per band count, 1 ..= [`MAX_MIX_BANDS`], started once per test
/// (index = bands − 1).
fn pools() -> Vec<BandPool> {
    (1..=MAX_MIX_BANDS)
        .map(|bands| BandPool::new("mix-test", bands))
        .collect()
}

/// A side made a whole `dst` picture, as the two-pass sender made it.
fn whole(side: Side<'_>, dst: Layout) -> Vec<u8> {
    let mut out = Vec::new();
    match side {
        Side::Black => black_nv12_into(dst, &mut out),
        Side::Same(picture) => out.extend_from_slice(picture),
        Side::Fitted(plan, src) => plan.apply(src, &mut out),
    }
    out
}

/// The two-pass reference: each side a whole `dst` picture, then the blend
/// (a plain fit: its side alone), at most `dst`'s bytes.
fn reference(dst: Layout, paint: Paint<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    match paint {
        Paint::Fit(side) => out = whole(side, dst),
        Paint::Fade { from, to, weight } => {
            blend_nv12_into(&whole(from, dst), &whole(to, dst), weight, &mut out)
        }
    }
    out.truncate(dst.len);
    out
}

/// The fused paint on `pool`, checking that it appends.
fn mixed(dst: Layout, paint: Paint<'_>, pool: &BandPool) -> Vec<u8> {
    let mut out = vec![9u8];
    mix_nv12_into(dst, paint, pool, &mut out);
    assert_eq!(
        out.remove(0),
        9,
        "the paint appends after what the buffer holds"
    );
    out
}

/// A fade of `from` and the incoming picture `to`, already in the layout.
fn over_same<'a>(from: Side<'a>, to: &'a [u8], weight: u32) -> Paint<'a> {
    Paint::Fade {
        from,
        to: Side::Same(to),
        weight,
    }
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
    assert_eq!(MIX_THREAD_NAME, "program-mix");
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
    let pools = pools();
    let mut bytes = Bytes(215);
    for (src_layout, dst_layout) in pairs {
        let src = bytes.frame(src_layout.len);
        let to = bytes.frame(dst_layout.len);
        let plan = FitPlan::new(src_layout, dst_layout);
        let random = [(bytes.next() % 257) as u32, (bytes.next() % 257) as u32];
        for weight in WEIGHTS.into_iter().chain(random) {
            let paint = over_same(Side::Fitted(&plan, &src), &to, weight);
            let want = reference(dst_layout, paint);
            assert_eq!(want.len(), dst_layout.len);
            for pool in &pools {
                assert_same(
                    &mixed(dst_layout, paint, pool),
                    &want,
                    &format!(
                        "{src_layout:?} into {dst_layout:?}, weight {weight}, {} bands",
                        pool.bands()
                    ),
                );
            }
        }
    }
}

#[test]
fn the_one_pass_fade_equals_the_two_pass_fade_on_random_frames() {
    // #223 follow-up: BOTH sides read in the one pass. The canvas is 1920×1080
    // at a tenth; the sources the catalog's sizes at a tenth (2560×1440,
    // 2560×1080, 1280×720), a 1920×1080 decoded on a padded stride, and a
    // canvas picture. Every pair of sides, the black included, at every
    // weight and band count.
    let dst = nv12(192, 108, 192);
    let pools = pools();
    let mut bytes = Bytes(5_973_498_519);
    let layouts = [
        nv12(256, 144, 256),
        nv12(256, 108, 256),
        nv12(128, 72, 128),
        nv12(192, 108, 200),
    ];
    let sources: Vec<(FitPlan, Vec<u8>)> = layouts
        .iter()
        .map(|&layout| (FitPlan::new(layout, dst), bytes.frame(layout.len)))
        .collect();
    let canvas_picture = bytes.frame(dst.len);
    let mut sides = vec![
        ("black", Side::Black),
        ("same", Side::Same(&canvas_picture)),
    ];
    for (plan, src) in &sources {
        sides.push(("fitted", Side::Fitted(plan, src)));
    }
    // Each side made a whole canvas picture once (the two-pass sender's
    // first pass), blended per pair and weight below.
    let wholes: Vec<Vec<u8>> = sides.iter().map(|&(_, side)| whole(side, dst)).collect();
    for (i, &(from_kind, from)) in sides.iter().enumerate() {
        for (j, &(to_kind, to)) in sides.iter().enumerate() {
            for weight in WEIGHTS {
                let paint = Paint::Fade { from, to, weight };
                let mut want = Vec::new();
                blend_nv12_into(&wholes[i], &wholes[j], weight, &mut want);
                assert_eq!(want.len(), dst.len, "the canvas's bytes");
                for pool in &pools {
                    assert_same(
                        &mixed(dst, paint, pool),
                        &want,
                        &format!(
                            "{from_kind} side {i} → {to_kind} side {j}, weight {weight}, {} bands",
                            pool.bands()
                        ),
                    );
                }
            }
        }
    }
}

#[test]
fn a_fade_between_two_fitted_sizes_is_exact() {
    // A 4×2 picture fills an 8×4 canvas (scaled up 2×); a 6×4 one sits at
    // x = 0 with a 2-column bar on its right. Exact bytes from the scratch
    // model, each side fitted as it is read, the bars blended too.
    let dst = nv12(8, 4, 8);
    let small: [u8; 12] = [16, 32, 64, 100, 128, 200, 235, 0, 128, 128, 90, 240];
    let tall: Vec<u8> = (0..36u32).map(|i| ((7 * i + 3) % 256) as u8).collect();
    let (small_plan, tall_plan) = (
        FitPlan::new(nv12(4, 2, 4), dst),
        FitPlan::new(nv12(6, 4, 6), dst),
    );
    assert_eq!(small_plan.rect(), (0, 0, 8, 4), "the 4×2 picture fills it");
    assert_eq!(tall_plan.rect(), (0, 0, 6, 4), "the 6×4 one: a bar of 2");
    let (small, tall) = (
        Side::Fitted(&small_plan, &small),
        Side::Fitted(&tall_plan, &tall),
    );
    let pools = pools();
    let cases: [(Paint<'_>, [u8; 48], &str); 5] = [
        (
            Paint::Fade {
                from: small,
                to: tall,
                weight: 100,
            },
            [
                11, 16, 24, 34, 46, 59, 62, 67, 44, 52, 64, 76, 89, 92, 57, 52, 95, 107, 127, 144,
                157, 139, 47, 21, 128, 142, 167, 186, 199, 171, 42, 6, 145, 148, 145, 170, 139,
                210, 105, 196, 161, 164, 161, 186, 155, 226, 105, 196,
            ],
            "4×2 → 6×4 at 100/256",
        ),
        (
            Paint::Fade {
                from: tall,
                to: small,
                weight: 100,
            },
            [
                8, 14, 21, 30, 41, 52, 45, 49, 45, 52, 62, 72, 83, 87, 42, 39, 92, 102, 118, 131,
                142, 133, 36, 20, 129, 140, 158, 173, 184, 169, 33, 10, 154, 158, 159, 178, 160,
                208, 113, 172, 180, 184, 185, 204, 186, 234, 113, 172,
            ],
            "6×4 → 4×2 at 100/256",
        ),
        (
            Paint::Fade {
                from: Side::Black,
                to: tall,
                weight: 14,
            },
            [
                15, 16, 16, 16, 17, 17, 16, 16, 18, 18, 18, 19, 19, 20, 16, 16, 20, 20, 21, 21, 21,
                22, 16, 16, 22, 23, 23, 23, 24, 24, 16, 16, 130, 131, 131, 132, 132, 132, 128, 128,
                133, 133, 133, 134, 134, 135, 128, 128,
            ],
            "a fade up from the black at 14/256",
        ),
        (
            Paint::Fit(small),
            [
                16, 20, 28, 40, 56, 73, 91, 100, 44, 52, 67, 82, 99, 99, 83, 75, 100, 115, 144,
                167, 184, 150, 67, 25, 128, 146, 182, 209, 226, 176, 59, 0, 128, 128, 119, 156,
                100, 212, 90, 240, 128, 128, 119, 156, 100, 212, 90, 240,
            ],
            "the 4×2 picture fitted alone",
        ),
        (
            Paint::Fit(tall),
            [
                3, 10, 17, 24, 31, 38, 16, 16, 45, 52, 59, 66, 73, 80, 16, 16, 87, 94, 101, 108,
                115, 122, 16, 16, 129, 136, 143, 150, 157, 164, 16, 16, 171, 178, 185, 192, 199,
                206, 128, 128, 213, 220, 227, 234, 241, 248, 128, 128,
            ],
            "the 6×4 picture fitted alone, its bar black",
        ),
    ];
    for (paint, want, what) in cases {
        assert_same(
            &reference(dst, paint),
            &want,
            &format!("{what}: the two-pass reference"),
        );
        for pool in &pools {
            assert_same(
                &mixed(dst, paint, pool),
                &want,
                &format!("{what}, {} bands", pool.bands()),
            );
        }
    }
}

#[test]
fn a_plain_fit_is_its_one_side_alone() {
    // A plain fit reads ONE side: the fitted picture, a picture already in
    // the layout (no longer than it), or the black.
    let dst = nv12(64, 36, 64);
    let pools = pools();
    let mut bytes = Bytes(1_920);
    let src_layout = nv12(48, 40, 56);
    let src = bytes.frame(src_layout.len);
    let plan = FitPlan::new(src_layout, dst);
    let mut fitted = Vec::new();
    plan.apply(&src, &mut fitted);
    let same = bytes.frame(dst.len + 7);
    let mut black = Vec::new();
    black_nv12_into(dst, &mut black);
    for (paint, want, what) in [
        (Paint::Fit(Side::Fitted(&plan, &src)), &fitted[..], "fitted"),
        (
            Paint::Fit(Side::Same(&same)),
            &same[..dst.len],
            "as it is, cut to the layout",
        ),
        (
            Paint::Fit(Side::Same(&same[..20])),
            &same[..20],
            "a shorter picture: its bytes",
        ),
        (Paint::Fit(Side::Black), &black[..], "the black"),
    ] {
        for pool in &pools {
            assert_same(
                &mixed(dst, paint, pool),
                want,
                &format!("{what}, {} bands", pool.bands()),
            );
        }
    }
}

#[test]
#[should_panic(expected = "a fitted side's plan fits into the paint's layout")]
fn a_fitted_side_whose_plan_fits_another_layout_panics() {
    // Its rows would not be the paint's: a wrong picture labelled as the
    // layout, never painted.
    let plan = FitPlan::new(nv12(4, 2, 4), nv12(8, 2, 8));
    let pool = BandPool::new("mix-test", 1);
    mixed(
        nv12(8, 4, 8),
        Paint::Fit(Side::Fitted(&plan, &[16u8; 12])),
        &pool,
    );
}

#[test]
fn the_equal_layout_mix_equals_the_blend_on_random_frames() {
    let pools = pools();
    let mut bytes = Bytes(5_858_472_395);
    for layout in [nv12(64, 36, 64), nv12(64, 36, 72), nv12(48, 40, 48)] {
        let from = bytes.frame(layout.len);
        let to = bytes.frame(layout.len);
        for weight in WEIGHTS {
            let paint = over_same(Side::Same(&from), &to, weight);
            let want = reference(layout, paint);
            for pool in &pools {
                assert_same(
                    &mixed(layout, paint, pool),
                    &want,
                    &format!("{layout:?}, weight {weight}, {} bands", pool.bands()),
                );
            }
        }
    }
}

#[test]
fn every_band_count_paints_what_one_band_paints_whatever_the_row_count() {
    // Row counts that 2..6 do not divide, and pictures of fewer rows than
    // bands (a band may get no rows at all).
    let pools = pools();
    let mut bytes = Bytes(33);
    let src_layout = nv12(9, 7, 10);
    let src = bytes.frame(src_layout.len);
    let tall_layout = nv12(5, 11, 6);
    let tall = bytes.frame(tall_layout.len);
    for height in [1, 2, 3, 5, 7, 11] {
        let dst_layout = nv12(6, height, 8);
        let to = bytes.frame(dst_layout.len);
        let plan = FitPlan::new(src_layout, dst_layout);
        let tall_plan = FitPlan::new(tall_layout, dst_layout);
        let same_picture = bytes.frame(dst_layout.len);
        for paint in [
            over_same(Side::Fitted(&plan, &src), &to, 90),
            over_same(Side::Same(&same_picture), &to, 90),
            Paint::Fade {
                from: Side::Fitted(&plan, &src),
                to: Side::Fitted(&tall_plan, &tall),
                weight: 90,
            },
        ] {
            let one = mixed(dst_layout, paint, &pools[0]);
            assert_same(&one, &reference(dst_layout, paint), "one band");
            for pool in &pools[1..] {
                assert_same(
                    &mixed(dst_layout, paint, pool),
                    &one,
                    &format!("{height} rows in {} bands", pool.bands()),
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
    let pools = [BandPool::new("mix-test", 1), BandPool::new("mix-test", 3)];
    let mut bytes = Bytes(1_000);
    for src_layout in SWEEP {
        for dst_layout in SWEEP {
            let plan = FitPlan::new(src_layout, dst_layout);
            let whole_src = bytes.frame(src_layout.len);
            let whole_to = bytes.frame(dst_layout.len);
            let short = |v: &[u8]| v[..v.len().saturating_sub(1)].to_vec();
            for src in [whole_src.clone(), short(&whole_src)] {
                for to in [whole_to.clone(), short(&whole_to)] {
                    let mut sides = vec![("fitted", Side::Fitted(&plan, &src))];
                    if src_layout == dst_layout {
                        sides.push(("same", Side::Same(&src)));
                    }
                    for (kind, from) in sides {
                        for weight in [0, 100, 256] {
                            // The incoming side as it is, and fitted too.
                            for (to_kind, to_side) in [
                                ("same", Side::Same(&to)),
                                ("fitted", Side::Fitted(&plan, &src)),
                                ("black", Side::Black),
                            ] {
                                let paint = Paint::Fade {
                                    from,
                                    to: to_side,
                                    weight,
                                };
                                let want = reference(dst_layout, paint);
                                for pool in &pools {
                                    assert_same(
                                        &mixed(dst_layout, paint, pool),
                                        &want,
                                        &format!(
                                            "{kind} {src_layout:?} ({} bytes) → {to_kind} into \
                                             {dst_layout:?} ({} bytes), weight {weight}, {} bands",
                                            src.len(),
                                            to.len(),
                                            pool.bands()
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
}

#[test]
fn every_band_beyond_the_first_is_painted_on_a_helper_thread() {
    // The fix of box run 2 (the fit and the blend ran on the one
    // `SP-program` thread): a picture in K bands is painted by K threads,
    // the calling one and the pool's worker of each further band, even when
    // some bands get no rows. Where they run, picture after picture, is
    // pinned in `band_pool_tests.rs`.
    let mut bytes = Bytes(7);
    let (src_layout, dst_layout) = (nv12(64, 36, 64), nv12(48, 40, 48));
    let (src, to) = (bytes.frame(src_layout.len), bytes.frame(dst_layout.len));
    let plan = FitPlan::new(src_layout, dst_layout);
    let same = bytes.frame(dst_layout.len);
    let flat = nv12(6, 2, 6);
    let flat_picture = bytes.frame(flat.len);
    for pool in pools() {
        let bands = pool.bands();
        let mut out = Vec::new();
        assert_eq!(
            mix_nv12_into(
                dst_layout,
                over_same(Side::Fitted(&plan, &src), &to, 128),
                &pool,
                &mut out
            ),
            bands,
            "the fitted mix in {bands} bands"
        );
        assert_eq!(
            mix_nv12_into(
                dst_layout,
                over_same(Side::Same(&same), &to, 128),
                &pool,
                &mut out
            ),
            bands,
            "the equal-layout mix in {bands} bands"
        );
        assert_eq!(
            mix_nv12_into(
                flat,
                over_same(Side::Same(&flat_picture), &flat_picture, 128),
                &pool,
                &mut out
            ),
            bands,
            "a 2-row picture in {bands} bands"
        );
        assert_eq!(
            mix_nv12_into(
                dst_layout,
                Paint::Fit(Side::Fitted(&plan, &src)),
                &pool,
                &mut out
            ),
            bands,
            "a plain fit in {bands} bands"
        );
    }
}

#[test]
fn a_run_may_start_anywhere_in_a_row() {
    // The bands start on row edges, but the painter must not rely on it:
    // cutting the destination anywhere (mid-row, inside the bars, inside
    // the picture, on the plane edge) paints the same bytes, for every kind
    // of side on either end of a fade.
    let mut bytes = Bytes(99);
    let (src_layout, dst_layout) = (nv12(10, 4, 12), nv12(16, 6, 18));
    let (src, to) = (bytes.frame(src_layout.len), bytes.frame(dst_layout.len));
    let plan = FitPlan::new(src_layout, dst_layout);
    assert_eq!(plan.rect(), (0, 0, 14, 6), "a pillarbox: bars right of it");
    let wide_layout = nv12(20, 4, 20);
    let wide = bytes.frame(wide_layout.len);
    let wide_plan = FitPlan::new(wide_layout, dst_layout);
    assert_eq!(
        wide_plan.rect(),
        (0, 2, 16, 2),
        "a letterbox: bars above and below it"
    );
    let same = bytes.frame(dst_layout.len);
    for (kind, paint) in [
        ("fitted", over_same(Side::Fitted(&plan, &src), &to, 77)),
        ("same", over_same(Side::Same(&same), &to, 77)),
        (
            "fitted → fitted",
            Paint::Fade {
                from: Side::Fitted(&plan, &src),
                to: Side::Fitted(&wide_plan, &wide),
                weight: 77,
            },
        ),
        (
            "black → fitted",
            Paint::Fade {
                from: Side::Black,
                to: Side::Fitted(&wide_plan, &wide),
                weight: 77,
            },
        ),
        (
            "fitted → black",
            Paint::Fade {
                from: Side::Fitted(&wide_plan, &wide),
                to: Side::Black,
                weight: 77,
            },
        ),
        ("a plain fit", Paint::Fit(Side::Fitted(&plan, &src))),
    ] {
        let want = reference(dst_layout, paint);
        for cuts in [
            vec![5, 23, 40, 107, 108, 115, 130],
            vec![1, 2, 3, 17, 19, 33, 35, 150, 161],
            vec![13, 14, 15, 16, 17, 100, 120, 145],
        ] {
            let mix = Mix::new(dst_layout, paint);
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
