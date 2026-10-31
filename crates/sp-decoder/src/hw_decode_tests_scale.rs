//! #282: the decode cap's decisions (`hw_decode.rs`), on Linux: the box, the
//! scaled size, the readback's reference size, whether Media Foundation took
//! the scaled open, the output type's pixel aspect, and the path of a scaled
//! picture. Wired via
//! `#[cfg(test)] #[path = "hw_decode_tests_scale.rs"] mod tests_scale;`.
//! Every pin was derived from a scratch model of the same arithmetic, which
//! also checked that each listed mutant fails one of these tests.

use super::{
    D3D11_BIND_DECODER, DecodePath, VideoArea, decode_box, decoder_path_visible,
    padding_reaches_processor, scale_took, scaled_frame_rate, scaled_open_safe,
    scaled_pixel_aspect, scaled_size, video_area, visible_reference,
};

/// One cap's box: (cap, (width, height)).
type BoxCase = (u32, (u64, u64));

#[test]
fn the_box_is_the_caps_16_9_rounded_down_to_even() {
    let cases: [BoxCase; 9] = [
        (2160, (3840, 2160)),
        (1440, (2560, 1440)),
        (1080, (1920, 1080)),
        (720, (1280, 720)),
        // 853.3 → 853 → 852.
        (480, (852, 480)),
        // An odd cap through the API: 1921.7 → 1920, and 1081 → 1080.
        (1081, (1920, 1080)),
        (2, (2, 2)),
        (1, (0, 0)),
        (0, (0, 0)),
    ];
    for (cap, expected) in cases {
        assert_eq!(decode_box(cap), expected, "cap {cap}");
    }
}

/// One scaled open: (native size, cap, decoded size).
type ScaleCase = ((u32, u32), u32, Option<(u32, u32)>);

#[test]
fn a_picture_larger_than_the_box_is_scaled_into_it_keeping_its_aspect() {
    let cases: [ScaleCase; 14] = [
        // The design's examples.
        ((3840, 2160), 1080, Some((1920, 1080))),
        // 1606 / 2 = 803, a tie: rounded up to 804.
        ((3840, 1606), 1080, Some((1920, 804))),
        ((2880, 2160), 1440, Some((1920, 1440))),
        // Every cap of the setting.
        ((3840, 2160), 1440, Some((2560, 1440))),
        ((3840, 2160), 720, Some((1280, 720))),
        // 1070.67 → 1070.
        ((3840, 1606), 1440, Some((2560, 1070))),
        // DCI 4K is wider than the 2160 box: its width binds.
        ((4096, 2160), 2160, Some((3840, 2026))),
        // 1012.5 → 1012.
        ((2560, 1350), 1080, Some((1920, 1012))),
        // Portrait: the height binds.
        ((1440, 1920), 1080, Some((810, 1080))),
        // Odd sizes: the side that does not bind is rounded to the nearest
        // even (1919.2 → 1920; 1918.2 → 1918; 1079.4 → 1080).
        ((1921, 1081), 1080, Some((1920, 1080))),
        ((1920, 1081), 1080, Some((1918, 1080))),
        ((1921, 1080), 1080, Some((1920, 1080))),
        // Over the box's width by 2: 478.9 → 478.
        ((854, 480), 480, Some((852, 478))),
        // The Windows CI fixture under a small cap.
        ((160, 120), 90, Some((120, 90))),
    ];
    for (native, cap, expected) in cases {
        assert_eq!(scaled_size(native, cap), expected, "{native:?} at {cap}");
    }
}

#[test]
fn a_picture_inside_the_box_is_never_scaled_nor_upscaled() {
    let cases: [ScaleCase; 7] = [
        ((1920, 1080), 1080, None),
        ((3840, 2160), 2160, None),
        ((3840, 1606), 2160, None),
        ((2560, 1350), 1440, None),
        ((852, 480), 480, None),
        ((160, 120), 120, None),
        ((1280, 720), 2160, None),
    ];
    for (native, cap, expected) in cases {
        assert_eq!(scaled_size(native, cap), expected, "{native:?} at {cap}");
    }
}

#[test]
fn every_side_is_even_and_at_least_2() {
    let cases: [ScaleCase; 4] = [
        // 0.17 rows → 0 → 2.
        ((10_000, 2), 480, Some((852, 2))),
        ((2, 10_000), 480, Some((2, 480))),
        // The smallest box that caps anything: 2×2.
        ((3840, 2160), 2, Some((2, 2))),
        ((160, 120), 60, Some((80, 60))),
    ];
    for (native, cap, expected) in cases {
        assert_eq!(scaled_size(native, cap), expected, "{native:?} at {cap}");
    }
    for native in [(3840, 2160), (3840, 1606), (4096, 2160), (1921, 1081)] {
        for cap in [2160, 1440, 1080, 720, 480, 1081] {
            if let Some((w, h)) = scaled_size(native, cap) {
                assert!(
                    w.is_multiple_of(2) && h.is_multiple_of(2),
                    "{native:?} at {cap}: {w}x{h}"
                );
                let (bw, bh) = decode_box(cap);
                assert!(
                    u64::from(w) <= bw && u64::from(h) <= bh,
                    "{native:?} at {cap}"
                );
                assert!(w <= native.0 && h <= native.1, "never upscaled");
            }
        }
    }
}

#[test]
fn an_unknown_size_or_a_cap_under_2_rows_caps_nothing() {
    let cases: [ScaleCase; 4] = [
        ((0, 2160), 1080, None),
        ((3840, 0), 1080, None),
        ((3840, 2160), 1, None),
        ((3840, 2160), 0, None),
    ];
    for (native, cap, expected) in cases {
        assert_eq!(scaled_size(native, cap), expected, "{native:?} at {cap}");
    }
}

#[test]
fn the_largest_sizes_do_not_overflow() {
    assert_eq!(scaled_size((u32::MAX, u32::MAX), 2160), Some((2160, 2160)));
    assert_eq!(
        scaled_size((u32::MAX, u32::MAX), u32::MAX),
        Some((u32::MAX - 1, u32::MAX - 1))
    );
}

/// The bug a native reference would cause: a 1920×804 scaled surface never
/// holds the 3840×1606 native rect, so the readback would hand over the
/// whole (maybe padded) surface; with the requested size it cuts the
/// padding exactly as it does for an unscaled file.
#[test]
fn a_scaled_open_cuts_its_surface_to_the_requested_size() {
    assert_eq!(visible_reference(None, None), None);
    assert_eq!(
        visible_reference(Some((3840, 1606)), None),
        Some((3840, 1606)),
        "unscaled: the stream's own size, as before"
    );
    assert_eq!(
        visible_reference(Some((3840, 1606)), Some((1920, 804))),
        Some((1920, 804))
    );
    assert_eq!(
        visible_reference(None, Some((1920, 804))),
        Some((1920, 804))
    );
    let reference = visible_reference(Some((3840, 1606)), Some((1920, 804)));
    assert_eq!(super::visible_size((1920, 816), reference), (1920, 804));
}

/// One scaled open's negotiated type: (requested, negotiated, taken).
type TookCase = ((u32, u32), (u32, u32), bool);

/// Review round 1: behind the video processor the output type is ours, so
/// only the exact size counts; a padded type would be picture the readback
/// cuts off (a silent crop), on the D3D11 path as in software.
#[test]
fn a_scaled_open_counts_only_when_media_foundation_negotiated_the_exact_size() {
    let cases: [TookCase; 8] = [
        ((1920, 1080), (1920, 1080), true),
        ((1920, 804), (1920, 804), true),
        // A padded type: refused, never cut.
        ((1920, 804), (1920, 816), false),
        ((1920, 804), (1935, 819), false),
        ((1920, 804), (1920, 805), false),
        ((1920, 804), (1921, 804), false),
        // A reader that kept the native size.
        ((1920, 1080), (3840, 2160), false),
        // Smaller than asked.
        ((1920, 1080), (1918, 1080), false),
    ];
    for (requested, negotiated, took) in cases {
        assert_eq!(
            scale_took(requested, negotiated),
            took,
            "{requested:?} → {negotiated:?}"
        );
    }
}

/// Review round 1: a scaled picture's surface is the video processor's, so
/// it tells nothing of the decoder's path: no GPU decode (and no software
/// one) is counted for it.
#[test]
fn only_an_unscaled_d3d_picture_tells_the_decoders_path() {
    assert!(decoder_path_visible(true, false));
    assert!(!decoder_path_visible(true, true));
    assert!(!decoder_path_visible(false, false));
    assert!(!decoder_path_visible(false, true));
}

/// One output pixel aspect: (native, native ratio, scaled, ratio).
type ParCase = (
    (u32, u32),
    Option<(u32, u32)>,
    (u32, u32),
    Option<(u32, u32)>,
);

/// Review round 1: the scaled size is a fraction of a pixel off the native
/// aspect; the output ratio states the display aspect exactly (whether
/// the reader keeps it: `scaled_pixel_aspect`'s doc).
#[test]
fn a_scaled_output_keeps_the_native_display_aspect() {
    let cases: [ParCase; 13] = [
        ((3840, 2160), None, (1920, 1080), Some((1, 1))),
        ((3840, 1606), None, (1920, 804), Some((804, 803))),
        ((4096, 2160), None, (3840, 2026), Some((2026, 2025))),
        ((2560, 1350), None, (1920, 1012), Some((2024, 2025))),
        // The Windows CI fixture at a cap of 50.
        ((160, 120), None, (66, 50), Some((100, 99))),
        // An anamorphic source keeps its own ratio.
        ((1440, 1080), Some((4, 3)), (960, 720), Some((4, 3))),
        // A ratio with a zero term is square pixels.
        ((3840, 1606), Some((0, 1)), (1920, 804), Some((804, 803))),
        ((3840, 1606), Some((1, 0)), (1920, 804), Some((804, 803))),
        // No size: no ratio.
        ((0, 1080), None, (1920, 1080), None),
        ((3840, 0), None, (1920, 1080), None),
        ((3840, 2160), None, (0, 1080), None),
        // A reduced term over a `u32`: no ratio.
        ((u32::MAX, 1), Some((u32::MAX, 1)), (1, 1), None),
        // Reduced by 3: 4294967295 = 3 × 1431655765.
        ((u32::MAX, 3), None, (2, 1), Some((1_431_655_765, 2))),
    ];
    for (native, par, scaled, expected) in cases {
        assert_eq!(
            scaled_pixel_aspect(native, par, scaled),
            expected,
            "{native:?} at {par:?} → {scaled:?}"
        );
    }
}

#[test]
fn a_scaled_pictures_surface_is_the_gpus_whatever_its_bind_flags() {
    // The video processor's output texture: render target / shader
    // resource, no BIND_DECODER.
    assert_eq!(DecodePath::of_surface(0x28, true), DecodePath::Hardware);
    assert_eq!(DecodePath::of_surface(0, true), DecodePath::Hardware);
    assert_eq!(
        DecodePath::of_surface(D3D11_BIND_DECODER, true),
        DecodePath::Hardware
    );
    // Unscaled: the decoder texture rule, as before.
    assert_eq!(
        DecodePath::of_surface(D3D11_BIND_DECODER, false),
        DecodePath::Hardware
    );
    assert_eq!(DecodePath::of_surface(0x28, false), DecodePath::Software);
    assert_eq!(DecodePath::of_surface(0, false), DecodePath::Software);
}

/// Review round 2: a scaled open sets no rate of its own (that could start
/// the processor's frame-rate conversion); a processor type without one
/// reports the native rate, never the 29.97 guess.
#[test]
fn a_scaled_reader_reports_the_negotiated_rate_else_the_native_one() {
    // 30000/1001 and 25/1, packed as Media Foundation packs them.
    let ntsc = (30_000_u64 << 32) | 1_001;
    let pal = (25_u64 << 32) | 1;
    assert_eq!(scaled_frame_rate(Some(pal), Some(ntsc), true), Some(pal));
    assert_eq!(scaled_frame_rate(Some(pal), None, false), Some(pal));
    assert_eq!(scaled_frame_rate(None, Some(ntsc), true), Some(ntsc));
    assert_eq!(
        scaled_frame_rate(None, Some(ntsc), false),
        None,
        "unscaled: the 29.97 guess, as before"
    );
    assert_eq!(scaled_frame_rate(None, None, true), None);
}

#[test]
fn an_mf_video_area_is_read_from_its_blob() {
    // OffsetX {fract 0x8000, value 2}, OffsetY {fract 0, value -3},
    // SIZE {3840, 1606}, little-endian.
    let blob: [u8; 16] = [
        0x00, 0x80, 0x02, 0x00, 0x00, 0x00, 0xfd, 0xff, 0x00, 0x0f, 0x00, 0x00, 0x46, 0x06, 0x00,
        0x00,
    ];
    assert_eq!(
        video_area(&blob),
        Some(VideoArea {
            x: 2,
            y: -3,
            width: 3840,
            height: 1606,
        })
    );
    assert_eq!(video_area(&blob[..15]), None, "too short");
    assert_eq!(video_area(&[0; 17]), None, "too long");
}

/// Review round 2 (#223's green strip): NVDEC pads its output to 16 rows;
/// the video processor crops it only by the decoder's display aperture, so
/// a padded type without that aperture would scale green rows into the
/// picture: the scaled open is refused.
#[test]
fn a_padded_decoder_picture_is_scaled_only_with_its_aperture() {
    let native = (3840, 1606);
    let exact = VideoArea {
        x: 0,
        y: 0,
        width: 3840,
        height: 1606,
    };
    // Not padded: nothing to crop.
    assert!(!padding_reaches_processor((3840, 1606), None, native));
    assert!(!padding_reaches_processor((3840, 1600), None, native));
    assert!(!padding_reaches_processor(
        (3840, 1606),
        Some(VideoArea {
            x: 2,
            y: 2,
            width: 100,
            height: 100,
        }),
        native
    ));
    // Padded rows or columns, no aperture: refused.
    assert!(padding_reaches_processor((3840, 1616), None, native));
    assert!(padding_reaches_processor((3856, 1606), None, native));
    // Padded with the exact aperture: the processor crops it.
    assert!(!padding_reaches_processor(
        (3840, 1616),
        Some(exact),
        native
    ));
    // Any other aperture: refused.
    for wrong in [
        VideoArea { x: 2, ..exact },
        VideoArea { y: 2, ..exact },
        VideoArea {
            width: 3830,
            ..exact
        },
        VideoArea {
            height: 1600,
            ..exact
        },
    ] {
        assert!(
            padding_reaches_processor((3840, 1616), Some(wrong), native),
            "{wrong:?}"
        );
    }
}

/// One scaled open's safety: (native, the decoder's output at open, on
/// the D3D11 path, may go on).
type SafeCase = (
    (u32, u32),
    Option<((u32, u32), Option<VideoArea>)>,
    bool,
    bool,
);

/// Review round 3: the decoder check runs at open, and on the D3D11 path
/// NVDEC may pad at the first frame, after it: a stream that is not whole
/// 16-pixel blocks is scaled there only once the decoder has shown the
/// native aperture; an unreadable chain refuses it too.
#[test]
fn a_scaled_open_goes_on_only_where_no_padding_can_reach_the_processor() {
    let exact = Some(VideoArea {
        x: 0,
        y: 0,
        width: 3840,
        height: 1606,
    });
    let off = |x: i16, y: i16, width: i32, height: i32| {
        Some(VideoArea {
            x,
            y,
            width,
            height,
        })
    };
    let cases: [SafeCase; 22] = [
        // The D3D11 path, the chain unreadable: whole blocks only.
        ((3840, 2160), None, true, true),
        ((3840, 1606), None, true, false),
        ((3848, 1600), None, true, false),
        // Software, the chain unreadable: Media Foundation's software
        // decoders hand over the stream's own size.
        ((3840, 1606), None, false, true),
        // The D3D11 path, read: whole blocks, or the aperture shown.
        ((3840, 2160), Some(((3840, 2160), None)), true, true),
        ((3840, 1606), Some(((3840, 1606), None)), true, false),
        ((3840, 1606), Some(((3840, 1616), exact)), true, true),
        ((3840, 1606), Some(((3840, 1606), exact)), true, true),
        ((3840, 1606), Some(((3840, 1616), None)), true, false),
        ((3848, 1600), Some(((3848, 1600), None)), true, false),
        (
            (3840, 1606),
            Some(((3840, 1616), off(2, 0, 3840, 1606))),
            true,
            false,
        ),
        (
            (3840, 1606),
            Some(((3840, 1616), off(0, 2, 3840, 1606))),
            true,
            false,
        ),
        (
            (3840, 1606),
            Some(((3840, 1616), off(0, 0, 3830, 1606))),
            true,
            false,
        ),
        (
            (3840, 1606),
            Some(((3840, 1616), off(0, 0, 3840, 1600))),
            true,
            false,
        ),
        ((3840, 1606), Some(((3856, 1606), None)), true, false),
        // Whole blocks, yet padded at open with no aperture: refused.
        ((3840, 2160), Some(((3840, 2176), None)), true, false),
        // Software, read: only the padding seen at open refuses.
        ((3840, 1606), Some(((3840, 1616), None)), false, false),
        ((3840, 1606), Some(((3856, 1606), None)), false, false),
        ((3840, 1606), Some(((3840, 1606), None)), false, true),
        // The CI fixture (160x120, coded 160x128).
        ((160, 120), Some(((160, 120), None)), false, true),
        (
            (160, 120),
            Some(((160, 128), off(0, 0, 160, 120))),
            false,
            true,
        ),
        ((160, 120), Some(((160, 128), None)), false, false),
    ];
    for (native, decoder, on_d3d, safe) in cases {
        assert_eq!(
            scaled_open_safe(native, decoder, on_d3d),
            safe,
            "{native:?} with {decoder:?}, d3d {on_d3d}"
        );
    }
}
