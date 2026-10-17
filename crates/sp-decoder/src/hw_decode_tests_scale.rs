//! #282: the decode cap's decisions (`hw_decode.rs`), on Linux: the box, the
//! scaled size, the readback's reference size, whether Media Foundation took
//! the scaled open, and the path of a scaled picture. Wired via
//! `#[cfg(test)] #[path = "hw_decode_tests_scale.rs"] mod tests_scale;`.
//! Every pin was derived from a scratch model of the same arithmetic, which
//! also checked that each listed mutant fails one of these tests.

use super::{
    D3D11_BIND_DECODER, DecodePath, decode_box, scale_took, scaled_size, visible_reference,
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

/// One scaled open's negotiated type: (requested, negotiated, on the D3D11
/// path, taken).
type TookCase = ((u32, u32), (u32, u32), bool, bool);

#[test]
fn a_scaled_open_counts_only_when_media_foundation_negotiated_the_size() {
    let cases: [TookCase; 12] = [
        ((1920, 1080), (1920, 1080), true, true),
        ((1920, 1080), (1920, 1080), false, true),
        // Padding the D3D11 readback cuts: up to 15 over on each side.
        ((1920, 804), (1920, 816), true, true),
        ((1920, 804), (1935, 819), true, true),
        ((1920, 804), (1936, 804), true, false),
        ((1920, 804), (1920, 820), true, false),
        // In software the picture is handed over uncut: exactly.
        ((1920, 804), (1920, 805), false, false),
        ((1920, 804), (1921, 804), false, false),
        // A reader that kept the native size.
        ((1920, 1080), (3840, 2160), true, false),
        ((1920, 1080), (3840, 2160), false, false),
        // Smaller than asked.
        ((1920, 1080), (1918, 1080), true, false),
        ((1920, 1080), (1920, 1078), true, false),
    ];
    for (requested, negotiated, on_d3d, took) in cases {
        assert_eq!(
            scale_took(requested, negotiated, on_d3d),
            took,
            "{requested:?} → {negotiated:?}, d3d {on_d3d}"
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
