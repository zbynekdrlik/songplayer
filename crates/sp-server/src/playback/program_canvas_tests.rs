//! #223: the `SP-program` canvas — its layout, which pictures already are
//! canvas pictures, the plans it keeps, the fit, and (#223 follow-up) a fade
//! boundary's picture painted from both sides (exact pins from a scratch
//! Python model of `FitPlan` + the Q8 blend; that it is ONE pass is
//! structural, `Mix::row`, and measured on the box).
//! Wired via `#[cfg(test)] #[path = "program_canvas_tests.rs"] mod tests;`.

use super::{Canvas, FIT_PLANS_KEPT};
use crate::playback::band_pool::BandPool;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_output::{PROGRAM_STANDBY_H, PROGRAM_STANDBY_W};
use crate::playback::program_transition::{Layout, MAX_MIX_BANDS};

fn layout(width: u32, height: u32, stride: u32, len: usize) -> Layout {
    Layout {
        width,
        height,
        stride,
        len,
    }
}

/// A tight `w`×`h` NV12 layout (stride `w`).
fn tight(w: u32, h: u32) -> Layout {
    layout(w, h, w, (w * h * 3 / 2) as usize)
}

/// A known 4×2 NV12 picture: 8 luma bytes, then 4 interleaved chroma bytes.
const FROM_4X2: [u8; 12] = [16, 32, 64, 100, 128, 200, 235, 0, 128, 128, 90, 240];

/// FROM_4X2 pillarboxed at x = 2 into the 8×2 canvas at scale 1, studio-black
/// bars around it.
const FROM_4X2_IN_8X2: [u8; 24] = [
    16, 16, 16, 32, 64, 100, 16, 16, 16, 16, 128, 200, 235, 0, 16, 16, 128, 128, 128, 128, 90, 240,
    128, 128,
];

#[test]
fn the_production_canvas_is_1920x1080_nv12_on_a_1920_stride() {
    let canvas = Canvas::new(PROGRAM_STANDBY_W, PROGRAM_STANDBY_H);
    assert_eq!(
        canvas.layout(),
        layout(1920, 1080, 1920, 3_110_400),
        "1920 × 1080 luma + 1920 × 540 chroma"
    );
    assert_eq!(
        Canvas::new(8, 2).layout(),
        layout(8, 2, 8, 24),
        "w·h·3/2 for any size"
    );
    assert_eq!(canvas.built(), 0, "no plan before a picture needs one");
}

#[test]
fn a_canvas_picture_has_its_size_and_stride_and_at_least_its_bytes() {
    let canvas = Canvas::new(8, 2);
    assert!(canvas.holds(layout(8, 2, 8, 24)), "exactly the canvas");
    assert!(
        canvas.holds(layout(8, 2, 8, 30)),
        "slack bytes past the picture"
    );
    assert!(!canvas.holds(layout(8, 2, 8, 23)), "one byte short");
    assert!(!canvas.holds(layout(8, 2, 10, 30)), "a padded stride");
    assert!(!canvas.holds(layout(6, 2, 8, 24)), "narrower");
    assert!(!canvas.holds(layout(8, 4, 8, 48)), "taller");
    assert!(!canvas.holds(layout(16, 4, 16, 96)), "larger");
}

#[test]
fn the_canvas_keeps_the_plans_used_last() {
    let (a, b, c) = (tight(4, 2), tight(6, 2), tight(8, 4));
    let mut canvas = Canvas::new(8, 2);
    let built = |canvas: &mut Canvas, src: Layout| {
        assert!(canvas.plan(src).fits(src, tight(8, 2)), "{src:?}'s plan");
        canvas.built()
    };
    assert_eq!(built(&mut canvas, a), 1);
    assert_eq!(built(&mut canvas, b), 2);
    assert_eq!(built(&mut canvas, a), 2, "A kept: a fade's two sides");
    assert_eq!(built(&mut canvas, b), 2, "B kept");
    assert_eq!(
        built(&mut canvas, a),
        2,
        "A again: A is now the one used last"
    );
    assert_eq!(
        built(&mut canvas, c),
        3,
        "C drops B, the plan used longest ago"
    );
    assert_eq!(built(&mut canvas, a), 3, "A was kept");
    assert_eq!(built(&mut canvas, b), 4, "B was dropped: built again");
    assert_eq!(FIT_PLANS_KEPT, 2);
}

#[test]
fn a_fit_sends_a_canvas_picture_as_it_is_and_fits_any_other() {
    for bands in 1..=MAX_MIX_BANDS {
        let pool = BandPool::new("canvas-test", bands);
        let mut canvas = Canvas::new(8, 2);
        let own = SharedFrame::new((0..24).collect());
        let same = canvas.fit(tight(8, 2), &own, &pool);
        assert!(same.ptr_eq(&own), "a canvas picture: the same allocation");
        assert_eq!(canvas.built(), 0);

        let small = SharedFrame::new(FROM_4X2.to_vec());
        let fitted = canvas.fit(tight(4, 2), &small, &pool);
        assert!(!fitted.ptr_eq(&small), "a buffer of its own");
        assert_eq!(
            fitted.to_vec(),
            FROM_4X2_IN_8X2.to_vec(),
            "pillarboxed at x = 2 at scale 1, studio-black bars, in {bands} bands"
        );
        assert_eq!(canvas.built(), 1);

        // Into a smaller canvas: scaled down (aspect 2:1 into 1:1, the 2×2
        // minimum fills it).
        let mut tiny = Canvas::new(2, 2);
        let fitted = tiny.fit(tight(4, 2), &small, &pool);
        assert_eq!(fitted.to_vec(), vec![24, 82, 164, 118, 109, 184]);
    }
}

#[test]
fn a_fit_reads_its_picture_alone_and_is_exactly_the_canvas_bytes() {
    // #223 follow-up: a plain fit reads ONE side, never a black buffer as a
    // second one, and is the canvas's bytes exactly (the sender labels it
    // with the canvas's size for the SDK). A picture short of its own layout
    // is not whole NV12: the canvas black, never a read past its end.
    let pool = BandPool::new("canvas-test", 2);
    let mut canvas = Canvas::new(8, 2);
    let short = SharedFrame::new(FROM_4X2[..11].to_vec());
    let fitted = canvas.fit(tight(4, 2), &short, &pool);
    let mut black = vec![16u8; 16];
    black.resize(24, 128);
    assert_eq!(fitted.to_vec(), black, "the 8×2 canvas's 24 black bytes");
}

#[test]
fn a_fade_paints_both_sides_in_the_canvas() {
    // The outgoing and the incoming side, each as it is when the canvas
    // holds it (slack bytes past it never painted), fitted into the canvas
    // as it is read when not, the canvas black when missing — blended at the
    // boundary's weight, exactly the canvas's bytes, in every band count.
    let own_bytes: Vec<u8> = (0..24).chain([99; 5]).collect();
    let own = (layout(8, 2, 8, 29), &own_bytes[..]);
    let small = (tight(4, 2), &FROM_4X2[..]);
    for bands in 1..=MAX_MIX_BANDS {
        let pool = BandPool::new("canvas-test", bands);
        let mut canvas = Canvas::new(8, 2);
        let cases = [
            (
                Some(own),
                Some(small),
                vec![
                    4, 5, 6, 10, 19, 29, 9, 9, 10, 11, 40, 58, 68, 10, 15, 15, 44, 45, 46, 46, 38,
                    76, 49, 49,
                ],
                "a canvas picture → a fitted one",
            ),
            (
                Some(small),
                None,
                vec![
                    16, 16, 16, 28, 52, 79, 16, 16, 16, 16, 100, 154, 180, 4, 16, 16, 128, 128,
                    128, 128, 100, 212, 128, 128,
                ],
                "a fitted picture → the missing side's black",
            ),
            (
                None,
                Some(own),
                vec![
                    12, 12, 13, 13, 13, 13, 14, 14, 14, 14, 15, 15, 15, 15, 16, 16, 100, 100, 101,
                    101, 101, 101, 102, 102,
                ],
                "the black → a canvas picture",
            ),
        ];
        for (from, to, want, what) in cases {
            let picture = canvas.fade(from, to, 64, &pool);
            assert_eq!(picture.to_vec(), want, "{what} at 64/256, in {bands} bands");
        }
        assert_eq!(canvas.built(), 1, "one plan, the 4×2 one, kept");
    }
}

#[test]
fn a_fade_between_two_sizes_keeps_both_plans() {
    // Both sides fitted on every boundary: two plans, built once each and
    // kept for the whole window (the scratch model's bytes: a 4×2 picture
    // filling an 8×4 canvas, a 6×4 one with a 2-column bar).
    let pool = BandPool::new("canvas-test", 3);
    let mut canvas = Canvas::new(8, 4);
    let tall: Vec<u8> = (0..36u32).map(|i| ((7 * i + 3) % 256) as u8).collect();
    let (small, tall) = ((tight(4, 2), &FROM_4X2[..]), (tight(6, 4), &tall[..]));
    for _ in 0..9 {
        let picture = canvas.fade(Some(small), Some(tall), 100, &pool);
        assert_eq!(
            picture.to_vec(),
            vec![
                11, 16, 24, 34, 46, 59, 62, 67, 44, 52, 64, 76, 89, 92, 57, 52, 95, 107, 127, 144,
                157, 139, 47, 21, 128, 142, 167, 186, 199, 171, 42, 6, 145, 148, 145, 170, 139,
                210, 105, 196, 161, 164, 161, 186, 155, 226, 105, 196,
            ]
        );
    }
    assert_eq!(canvas.built(), 2, "one plan per side, kept");
    // The outgoing side's plan is the one used last: a third size drops the
    // incoming side's.
    canvas.plan(tight(2, 2));
    assert_eq!(canvas.built(), 3);
    canvas.plan(tight(4, 2));
    assert_eq!(canvas.built(), 3, "the outgoing 4×2 plan was kept");
    canvas.plan(tight(6, 4));
    assert_eq!(canvas.built(), 4, "the incoming 6×4 plan was dropped");
}
