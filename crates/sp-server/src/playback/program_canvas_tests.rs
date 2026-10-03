//! #223: the `SP-program` canvas — its layout, which pictures already are
//! canvas pictures, the plans it keeps, and the fit (exact pins from a
//! scratch Python model of `FitPlan` + the Q8 blend at weight 0).
//! Wired via `#[cfg(test)] #[path = "program_canvas_tests.rs"] mod tests;`.

use super::{Canvas, FIT_PLANS_KEPT};
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
        let mut canvas = Canvas::new(8, 2);
        let black = {
            let mut black = vec![16u8; 16];
            black.resize(24, 128);
            black
        };
        let own = SharedFrame::new((0..24).collect());
        let same = canvas.fit(tight(8, 2), &own, &black, bands);
        assert!(same.ptr_eq(&own), "a canvas picture: the same allocation");
        assert_eq!(canvas.built(), 0);

        let small = SharedFrame::new(FROM_4X2.to_vec());
        let fitted = canvas.fit(tight(4, 2), &small, &black, bands);
        assert!(!fitted.ptr_eq(&small), "a buffer of its own");
        assert_eq!(
            fitted.to_vec(),
            vec![
                16, 16, 16, 32, 64, 100, 16, 16, 16, 16, 128, 200, 235, 0, 16, 16, 128, 128, 128,
                128, 90, 240, 128, 128
            ],
            "pillarboxed at x = 2 at scale 1, studio-black bars, in {bands} bands"
        );
        assert_eq!(canvas.built(), 1);

        // Into a smaller canvas: scaled down (aspect 2:1 into 1:1, the 2×2
        // minimum fills it).
        let mut tiny = Canvas::new(2, 2);
        let fitted = tiny.fit(tight(4, 2), &small, &[16, 16, 16, 16, 128, 128], bands);
        assert_eq!(fitted.to_vec(), vec![24, 82, 164, 118, 109, 184]);
    }
}
