//! Tests for [`aspect_fit`] (#223 S1a: moved here from `sp-server`
//! `playback::nv12_fit`, so the GPU compositor shares the rule). The values
//! come from a scratch model of the function; the preview's 640×360 cases
//! stay in `sp-server`'s `preview_stream_tests.rs` too.

use super::{Placement, aspect_fit};

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
