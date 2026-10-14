//! Tests for [`aspect_fit`] (#223 S1a: moved here from `sp-server`
//! `playback::nv12_fit`, so the GPU compositor shares the rule) and #267's
//! [`fill_width`] (MAX's rule). The values come from a scratch model of the
//! functions; the preview's 640×360 cases stay in `sp-server`'s
//! `preview_stream_tests.rs` too.

use super::{Fill, Placement, aspect_fit, fill_width};

fn place(w: u32, h: u32, off_x: u32, off_y: u32) -> Placement {
    Placement { w, h, off_x, off_y }
}

#[test]
fn a_16_by_9_source_fills_the_4k_canvas() {
    assert_eq!(aspect_fit(2560, 1440, 3840, 2160), place(3840, 2160, 0, 0));
    assert_eq!(aspect_fit(1280, 720, 3840, 2160), place(3840, 2160, 0, 0));
    assert_eq!(aspect_fit(7680, 4320, 3840, 2160), place(3840, 2160, 0, 0));
    assert_eq!(aspect_fit(1920, 1080, 1920, 1080), place(1920, 1080, 0, 0));
}

#[test]
fn a_21_by_9_source_gets_bars_top_and_bottom() {
    // 1080·3840/2560 = 1620 rows, centred: (2160 − 1620)/2 = 270.
    assert_eq!(
        aspect_fit(2560, 1080, 3840, 2160),
        place(3840, 1620, 0, 270)
    );
    // 2160·3840/4096 = 2025 → 2024 rows; (2160 − 2024)/2 = 68.
    assert_eq!(aspect_fit(4096, 2160, 3840, 2160), place(3840, 2024, 0, 68));
}

#[test]
fn a_4_by_3_source_gets_bars_left_and_right() {
    // 1440·2160/1080 = 2880 columns, centred: (3840 − 2880)/2 = 480.
    assert_eq!(
        aspect_fit(1440, 1080, 3840, 2160),
        place(2880, 2160, 480, 0)
    );
}

#[test]
fn sizes_and_offsets_are_floored_to_even() {
    // 720·360/1280 = 202 columns; (640 − 202)/2 = 219 → 218.
    assert_eq!(aspect_fit(720, 1280, 640, 360), place(202, 360, 218, 0));
    // 725·360/1280 = 203 → 202 columns.
    assert_eq!(aspect_fit(725, 1280, 640, 360), place(202, 360, 218, 0));
    // A 1-row source: 2 rows, (2160 − 2)/2 = 1079 → 1078.
    assert_eq!(aspect_fit(3841, 1, 3840, 2160), place(3840, 2, 0, 1078));
    // 5×3 into 7×5: 7 → 6 columns, 3·7/5 = 4 rows.
    assert_eq!(aspect_fit(5, 3, 7, 5), place(6, 4, 0, 0));
}

#[test]
fn an_extreme_aspect_keeps_at_least_two_pixels() {
    // 1·2160/720 = 3 → 2 columns (the floor to even, then the 2 minimum).
    assert_eq!(aspect_fit(1, 720, 3840, 2160), place(2, 2160, 1918, 0));
    // Under a 1×1 destination it overhangs at 2×2, offsets 0.
    assert_eq!(aspect_fit(2, 2, 1, 1), place(2, 2, 0, 0));
}

#[test]
fn a_degenerate_source_is_a_zero_image_at_the_even_centre() {
    // 10/2 = 5 → 4, 14/2 = 7 → 6.
    assert_eq!(aspect_fit(0, 2, 10, 14), place(0, 0, 4, 6));
    assert_eq!(aspect_fit(2, 0, 10, 14), place(0, 0, 4, 6));
    assert_eq!(aspect_fit(0, 0, 3840, 2160), place(0, 0, 1920, 1080));
}

fn fill(place: Placement, scaled_h: u32, crop_top: u32) -> Fill {
    Fill {
        place,
        scaled_h,
        crop_top,
    }
}

/// #267: the PP song (`MO3ucp2mBnM`, 2880×2160, 4:3) fills MAX's whole
/// width: 2880 scaled rows, 360 cut off the top and 360 off the bottom.
#[test]
fn a_4_by_3_picture_fills_the_whole_width_cropped_top_and_bottom() {
    let full = place(3840, 2160, 0, 0);
    assert_eq!(fill_width(2880, 2160, 3840, 2160), fill(full, 2880, 360));
    assert_eq!(fill_width(1440, 1080, 3840, 2160), fill(full, 2880, 360));
    // The same picture in a 1920×1080 target: 1440 rows, 180 cut each end.
    let fhd = place(1920, 1080, 0, 0);
    assert_eq!(fill_width(1440, 1080, 1920, 1080), fill(fhd, 1440, 180));
}

#[test]
fn a_square_or_portrait_picture_fills_the_whole_width_too() {
    let full = place(3840, 2160, 0, 0);
    // 1:1: 3840 rows, (3840 − 2160)/2 = 840 cut off each end.
    assert_eq!(fill_width(1080, 1080, 3840, 2160), fill(full, 3840, 840));
    // 9:16: 1920·3840/1080 = 6826 rows; (6826 − 2160)/2 = 2333 → 2332 off
    // the top (even), 2334 off the bottom.
    assert_eq!(fill_width(1080, 1920, 3840, 2160), fill(full, 6826, 2332));
    // 1 column: 720·3840 rows, the middle 2160 shown.
    assert_eq!(
        fill_width(1, 720, 3840, 2160),
        fill(full, 2_764_800, 1_381_320)
    );
}

#[test]
fn a_16_by_9_or_wider_picture_is_exactly_aspect_fit() {
    // 16:9: the whole canvas, nothing cut off.
    let full = place(3840, 2160, 0, 0);
    assert_eq!(fill_width(2560, 1440, 3840, 2160), Fill::whole(full));
    assert_eq!(fill_width(1280, 720, 3840, 2160), Fill::whole(full));
    let fhd = place(1920, 1080, 0, 0);
    assert_eq!(fill_width(1920, 1080, 1920, 1080), Fill::whole(fhd));
    // 21:9: the letterbox, bars top and bottom (rows 270..1890).
    assert_eq!(
        fill_width(2560, 1080, 3840, 2160),
        fill(place(3840, 1620, 0, 270), 1620, 0)
    );
    assert_eq!(
        fill_width(4096, 2160, 3840, 2160),
        fill(place(3840, 2024, 0, 68), 2024, 0)
    );
}

/// Every picture as wide as the destination or wider, in four
/// destinations: [`fill_width`] is [`aspect_fit`] with nothing cut off;
/// every narrower one covers the destination's whole even width and height.
#[test]
fn fill_width_is_aspect_fit_unless_the_picture_is_narrower() {
    let (mut wide, mut narrow) = (0, 0);
    for (dw, dh) in [(3840, 2160), (1920, 1080), (7, 5), (600, 800)] {
        for sw in (1..=97).step_by(4) {
            for sh in (1..=61).step_by(3) {
                let got = fill_width(sw, sh, dw, dh);
                if u64::from(sw) * u64::from(dh) >= u64::from(sh) * u64::from(dw) {
                    wide += 1;
                    let want = Fill::whole(aspect_fit(sw, sh, dw, dh));
                    assert_eq!(got, want, "{sw}x{sh} in {dw}x{dh}");
                } else {
                    narrow += 1;
                    let full = place(dw & !1, dh & !1, 0, 0);
                    assert_eq!(got.place, full, "{sw}x{sh} in {dw}x{dh}");
                    assert!(
                        got.crop_top + got.place.h <= got.scaled_h,
                        "{sw}x{sh} in {dw}x{dh}: {got:?}"
                    );
                }
            }
        }
    }
    assert!(wide > 100 && narrow > 100, "both kinds: {wide} / {narrow}");
}

#[test]
fn a_filled_picture_s_sizes_and_crop_are_even() {
    // 3×4 into 7×5: 6 columns, 4·7/3 = 9 → 8 rows, 5 → 4 shown, 2 cut off
    // the top.
    assert_eq!(fill_width(3, 4, 7, 5), fill(place(6, 4, 0, 0), 8, 2));
    // 1×1 into 7×5: 7 → 6 rows, 4 shown; (6 − 4)/2 = 1 → 0 cut off the top,
    // both cut rows at the bottom.
    assert_eq!(fill_width(1, 1, 7, 5), fill(place(6, 4, 0, 0), 6, 0));
    // 5×7 into 9×3: 7·9/5 = 12 rows, 3 → 2 shown, (12 − 2)/2 = 5 → 4.
    assert_eq!(fill_width(5, 7, 9, 3), fill(place(8, 2, 0, 0), 12, 4));
    // 720×1281 into 641×361: 640 columns, 1281·641/720 = 1140 rows, 360
    // shown, 390 cut off the top.
    assert_eq!(
        fill_width(720, 1281, 641, 361),
        fill(place(640, 360, 0, 0), 1140, 390)
    );
}

#[test]
fn a_filled_picture_keeps_at_least_two_pixels() {
    // Under a 1×1 destination it overhangs at 2×2, like `aspect_fit`.
    assert_eq!(fill_width(1, 2, 1, 1), fill(place(2, 2, 0, 0), 2, 0));
    // 1×3 into 2×2: 6 rows, the middle 2.
    assert_eq!(fill_width(1, 3, 2, 2), fill(place(2, 2, 0, 0), 6, 2));
}

#[test]
fn a_scaled_height_past_u32_is_capped() {
    // u32::MAX·3840 rows do not fit a u32: capped at u32::MAX, floored to
    // even, the middle 2160 rows shown.
    assert_eq!(
        fill_width(1, u32::MAX, 3840, 2160),
        fill(place(3840, 2160, 0, 0), 4_294_967_294, 2_147_482_566)
    );
}

#[test]
fn a_degenerate_source_fills_nothing() {
    // `aspect_fit`'s zero image at the even centre, nothing cut off.
    let zero = Fill::whole(place(0, 0, 4, 6));
    assert_eq!(zero, fill(place(0, 0, 4, 6), 0, 0));
    assert_eq!(fill_width(0, 2, 10, 14), zero);
    assert_eq!(fill_width(2, 0, 10, 14), zero);
    assert_eq!(
        fill_width(0, 0, 3840, 2160),
        Fill::whole(place(0, 0, 1920, 1080))
    );
}

#[test]
fn a_whole_fill_is_its_placement_with_nothing_cut_off() {
    let pillarbox = place(2880, 2160, 480, 0);
    assert_eq!(Fill::whole(pillarbox), fill(pillarbox, 2160, 0));
}
