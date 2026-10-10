//! WARP pixel pins of the `SP-program-MAX` compositor (#223 S1a, R3-2 "CI").
//!
//! Each test draws on WARP, Direct3D's CPU rasterizer (`windows-latest` has
//! no GPU), reads the render target back, and compares it with the CPU model
//! `sp_gpu::reference`: every colour channel within
//! `reference::tolerance` codes — one per quad covering the pixel, so the
//! bars exactly, a plain picture ±1, a fade's overlap ±2 (the proof is in
//! `reference.rs`) — and alpha exactly 255. That bound needs smooth pictures:
//! triangle waves of ≤ 12 luma codes per texel across (≤ 5 down) and ≤ 8
//! chroma codes per chroma texel. Every row carries 64 padding bytes of 0xFF
//! past its pixels: a stride bug would show them.
//!
//! Windows only. A WARP capability the compositor needs (the shared render
//! target included) that is missing FAILS here; nothing is skipped.

#![cfg(windows)]

mod common;

use common::{
    BLACK, H, W, assert_matches_reference, assert_matches_reference_in, at, at_in, pattern,
    picture, warp, warp_fhd,
};
use sp_gpu::{
    Composition, Compositor, FHD_HEIGHT, FHD_WIDTH, GpuError, Layer, PictureError, pick_adapter,
};
use windows::Win32::Graphics::Direct3D11::{D3D11_RESOURCE_MISC_SHARED, D3D11_TEXTURE2D_DESC};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

/// Every pixel outside `layer`'s quad is exactly the opaque black.
fn assert_bars_black(frame: &[u8], layer: &Layer<'_>, what: &str) {
    let p = layer.place;
    for y in 0..H {
        let inside_rows = y >= p.off_y && y < p.off_y + p.h;
        for x in 0..W {
            let inside = inside_rows && x >= p.off_x && x < p.off_x + p.w;
            if !inside {
                assert_eq!(at(frame, x, y), BLACK, "{what}: bar pixel ({x}, {y})");
            }
        }
    }
}

fn assert_opaque(frame: &[u8], what: &str) {
    let transparent = frame.chunks_exact(4).filter(|p| p[3] != 255).count();
    assert_eq!(transparent, 0, "{what}: pixels with alpha below 255");
}

/// Draw `composition` on a fresh WARP compositor and read it back.
fn draw(composition: &Composition<'_>) -> Vec<u8> {
    let mut compositor = warp();
    let stats = compositor.compose(composition).expect("compose on WARP");
    assert!(
        stats.draw_us > 0,
        "a 4K frame on WARP takes time: {stats:?}"
    );
    compositor.read_back().expect("read back on WARP")
}

#[test]
fn warp_builds_a_shared_bgra_4k_render_target() {
    let compositor = warp();
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { compositor.render_target().GetDesc(&mut desc) };
    assert_eq!((desc.Width, desc.Height), (W, H));
    assert_eq!(desc.Format, DXGI_FORMAT_B8G8R8A8_UNORM);
    assert_ne!(
        desc.MiscFlags & D3D11_RESOURCE_MISC_SHARED.0 as u32,
        0,
        "the render target is shared (Spout)"
    );
    let handle = compositor
        .shared_handle()
        .unwrap_or_else(|e| panic!("WARP must give the render target a shared handle: {e}"));
    assert!(!handle.is_invalid(), "shared handle {handle:?}");
    // WARP's device runs on the Basic Render Driver, which DXGI flags
    // software (`DXGI_ADAPTER_FLAG_SOFTWARE`, decoded by `info_of`).
    let adapter = compositor.adapter();
    assert!(adapter.software_flag, "WARP's adapter: {adapter:?}");
    assert_eq!((adapter.vendor_id, adapter.device_id), (0x1414, 0x8c));
}

#[test]
fn the_adapter_list_has_the_basic_render_driver_and_never_picks_it() {
    let adapters = sp_gpu::adapters().expect("DXGI lists its adapters");
    assert!(
        adapters
            .iter()
            .any(|a| a.vendor_id == 0x1414 && a.device_id == 0x8c && a.software_flag),
        "DXGI always lists the Basic Render Driver, flagged software: {adapters:?}"
    );
    if let Some(picked) = pick_adapter(&adapters) {
        assert!(!adapters[picked].is_software(), "{adapters:?}");
    }
}

#[test]
fn the_compositor_runs_on_a_listed_adapter_the_way_new_does() {
    // windows-latest has no GPU, so Compositor::new's device path (an explicit
    // DXGI adapter, D3D_DRIVER_TYPE_UNKNOWN) runs on the Basic Render Driver.
    let adapters = sp_gpu::adapters().expect("DXGI lists its adapters");
    let index = adapters
        .iter()
        .position(|a| a.vendor_id == 0x1414 && a.device_id == 0x8c)
        .expect("DXGI always lists the Basic Render Driver");
    let mut compositor = Compositor::new_on_listed_adapter(index)
        .unwrap_or_else(|e| panic!("a device on the listed Basic Render Driver: {e}"));
    assert_eq!(compositor.adapter(), &adapters[index]);
    let (stride, data) = pattern(1280, 720, 5);
    let composition = Composition::Picture(picture(1, 1280, 720, stride, &data));
    compositor
        .compose(&composition)
        .expect("compose on the listed adapter");
    let frame = compositor.read_back().expect("read back");
    assert_matches_reference(&frame, &composition.layers(), "listed adapter");
}

#[test]
fn new_takes_the_picked_adapter_or_reports_none() {
    let adapters = sp_gpu::adapters().expect("DXGI lists its adapters");
    match pick_adapter(&adapters) {
        None => assert_eq!(
            Compositor::new().err(),
            Some(GpuError::NoAdapter),
            "{adapters:?}"
        ),
        Some(index) => {
            let compositor = Compositor::new().unwrap_or_else(|e| {
                panic!(
                    "the picked adapter {:?} must run the compositor: {e}",
                    adapters[index]
                )
            });
            assert_eq!(compositor.adapter(), &adapters[index]);
        }
    }
}

#[test]
fn an_adapter_index_past_the_list_is_no_adapter() {
    let count = sp_gpu::adapters().expect("DXGI lists its adapters").len();
    assert_eq!(
        Compositor::new_on_listed_adapter(count).err(),
        Some(GpuError::NoAdapter)
    );
}

#[test]
fn the_device_lost_codes_are_the_sdk_values() {
    use windows::Win32::Graphics::Dxgi as dxgi;
    assert_eq!(
        sp_gpu::DXGI_ERROR_DEVICE_REMOVED,
        dxgi::DXGI_ERROR_DEVICE_REMOVED.0 as u32
    );
    assert_eq!(
        sp_gpu::DXGI_ERROR_DEVICE_HUNG,
        dxgi::DXGI_ERROR_DEVICE_HUNG.0 as u32
    );
    assert_eq!(
        sp_gpu::DXGI_ERROR_DEVICE_RESET,
        dxgi::DXGI_ERROR_DEVICE_RESET.0 as u32
    );
    assert_eq!(
        sp_gpu::DXGI_ERROR_DRIVER_INTERNAL_ERROR,
        dxgi::DXGI_ERROR_DRIVER_INTERNAL_ERROR.0 as u32
    );
}

#[test]
fn the_black_is_black_and_opaque_everywhere() {
    let frame = draw(&Composition::Black);
    assert_eq!(frame.len(), (W * H * 4) as usize);
    assert!(
        frame.chunks_exact(4).all(|p| p == BLACK),
        "the standby is all black"
    );
}

#[test]
fn a_1440p_picture_fills_the_canvas() {
    let (stride, data) = pattern(2560, 1440, 0);
    let composition = Composition::Picture(picture(1, 2560, 1440, stride, &data));
    let frame = draw(&composition);
    assert_matches_reference(&frame, &composition.layers(), "2560x1440");
    assert_opaque(&frame, "2560x1440");
}

#[test]
fn a_720p_picture_is_scaled_up_three_times() {
    let (stride, data) = pattern(1280, 720, 17);
    let composition = Composition::Picture(picture(1, 1280, 720, stride, &data));
    let frame = draw(&composition);
    assert_matches_reference(&frame, &composition.layers(), "1280x720");
    assert_opaque(&frame, "1280x720");
}

#[test]
fn a_21_by_9_picture_has_black_bars_top_and_bottom() {
    let (stride, data) = pattern(2560, 1080, 33);
    let composition = Composition::Picture(picture(1, 2560, 1080, stride, &data));
    let layers = composition.layers();
    assert_eq!((layers[0].place.off_y, layers[0].place.h), (270, 1620));
    let frame = draw(&composition);
    assert_bars_black(&frame, &layers[0], "2560x1080");
    assert_matches_reference(&frame, &layers, "2560x1080");
}

#[test]
fn a_4_by_3_picture_has_black_bars_left_and_right() {
    let (stride, data) = pattern(1440, 1080, 51);
    let composition = Composition::Picture(picture(1, 1440, 1080, stride, &data));
    let layers = composition.layers();
    assert_eq!((layers[0].place.off_x, layers[0].place.w), (480, 2880));
    let frame = draw(&composition);
    assert_bars_black(&frame, &layers[0], "1440x1080");
    assert_matches_reference(&frame, &layers, "1440x1080");
}

#[test]
fn a_half_way_fade_of_two_sizes_blends_both() {
    let (from_stride, from) = pattern(2560, 1440, 7);
    let (to_stride, to) = pattern(1440, 1080, 120);
    let composition = Composition::Fade {
        from: Some(picture(1, 2560, 1440, from_stride, &from)),
        to: Some(picture(2, 1440, 1080, to_stride, &to)),
        weight_q8: 128,
    };
    let frame = draw(&composition);
    // The incoming 4:3 picture's bars show the outgoing one at half weight.
    assert_matches_reference(&frame, &composition.layers(), "fade 50 %");
    assert_opaque(&frame, "fade 50 %");
}

#[test]
fn a_picture_with_the_same_id_is_not_uploaded_again() {
    let mut compositor = warp();
    let (stride, a) = pattern(1920, 1080, 3);
    let (_, b) = pattern(1920, 1080, 200);
    let (_, c) = pattern(1920, 1080, 77);

    let first = Composition::Picture(picture(1, 1920, 1080, stride, &a));
    let stats = compositor.compose(&first).expect("compose a");
    assert_eq!(stats.uploads, 1, "a new picture is uploaded");
    assert!(stats.upload_us > 0, "a 3 MB upload takes time: {stats:?}");
    let frame_a = compositor.read_back().expect("read back a");
    assert_matches_reference(&frame_a, &first.layers(), "picture a");

    // Other bytes under the same id: not uploaded, so `a` is still drawn.
    let same_id = Composition::Picture(picture(1, 1920, 1080, stride, &b));
    let stats = compositor.compose(&same_id).expect("compose b as id 1");
    assert_eq!(stats.uploads, 0, "the same id is not uploaded again");
    let again = compositor.read_back().expect("read back again");
    assert!(again == frame_a, "the resident picture is drawn again");

    // A new id is uploaded and drawn.
    let second = Composition::Picture(picture(2, 1920, 1080, stride, &b));
    assert_eq!(compositor.compose(&second).expect("compose b").uploads, 1);
    let frame_b = compositor.read_back().expect("read back b");
    assert_matches_reference(&frame_b, &second.layers(), "picture b");

    // A fade out of `b`: only the incoming side is new.
    let fade = Composition::Fade {
        from: Some(picture(2, 1920, 1080, stride, &b)),
        to: Some(picture(3, 1920, 1080, stride, &c)),
        weight_q8: 64,
    };
    assert_eq!(compositor.compose(&fade).expect("fade").uploads, 1);
    assert_eq!(compositor.compose(&fade).expect("fade again").uploads, 0);
    let frame_fade = compositor.read_back().expect("read back the fade");
    assert_matches_reference(&frame_fade, &fade.layers(), "fade b → c");
}

#[test]
fn an_invalid_picture_is_refused_before_anything_is_drawn() {
    let mut compositor = warp();
    let (stride, data) = pattern(1280, 720, 9);
    let good = Composition::Picture(picture(1, 1280, 720, stride, &data));
    compositor.compose(&good).expect("compose the good picture");
    let before = compositor.read_back().expect("read back");

    let short = &data[..data.len() - 1];
    let bad = Composition::Picture(picture(2, 1280, 720, stride, short));
    assert_eq!(
        compositor.compose(&bad),
        Err(GpuError::Picture(PictureError::Short {
            len: data.len() - 1,
            need: data.len(),
        }))
    );
    let after = compositor.read_back().expect("read back after the refusal");
    assert!(after == before, "a refused picture leaves the last frame");
}

/// #239: the `SP-program` sender's compositor has a shared 1920×1080 BGRA
/// target, and fits each picture into IT: a 1440p picture scaled down, a
/// 4:3 one with its bars, then a fade of the two, each against the
/// reference in its own target.
#[test]
fn a_1920_by_1080_compositor_draws_into_its_own_target() {
    let mut compositor = warp_fhd();
    assert_eq!(compositor.size(), (FHD_WIDTH, FHD_HEIGHT));
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { compositor.render_target().GetDesc(&mut desc) };
    assert_eq!((desc.Width, desc.Height), (1920, 1080));
    assert_eq!(desc.Format, DXGI_FORMAT_B8G8R8A8_UNORM);
    assert_ne!(
        desc.MiscFlags & D3D11_RESOURCE_MISC_SHARED.0 as u32,
        0,
        "the render target is shared (Spout)"
    );
    let size = (FHD_WIDTH, FHD_HEIGHT);
    let (big_stride, big) = pattern(2560, 1440, 7);
    let (narrow_stride, narrow) = pattern(1440, 1080, 120);

    let scaled = Composition::Picture(picture(1, 2560, 1440, big_stride, &big));
    compositor
        .compose(&scaled)
        .expect("compose 1440p into 1080p");
    let frame = compositor.read_back().expect("read back");
    assert_eq!(frame.len(), 1920 * 1080 * 4, "a 1080p frame");
    let layers = scaled.layers_in(FHD_WIDTH, FHD_HEIGHT);
    assert_matches_reference_in(&frame, &layers, size, "2560x1440 in 1920x1080");
    assert_opaque(&frame, "2560x1440 in 1920x1080");

    let pillarbox = Composition::Picture(picture(2, 1440, 1080, narrow_stride, &narrow));
    let layers = pillarbox.layers_in(FHD_WIDTH, FHD_HEIGHT);
    assert_eq!((layers[0].place.off_x, layers[0].place.w), (240, 1440));
    compositor.compose(&pillarbox).expect("compose 4:3");
    let frame = compositor.read_back().expect("read back");
    assert_eq!(at_in(&frame, FHD_WIDTH, 239, 540), BLACK, "the left bar");
    assert_eq!(at_in(&frame, FHD_WIDTH, 1680, 540), BLACK, "the right bar");
    assert_matches_reference_in(&frame, &layers, size, "1440x1080 in 1920x1080");

    let fade = Composition::Fade {
        from: Some(picture(1, 2560, 1440, big_stride, &big)),
        to: Some(picture(2, 1440, 1080, narrow_stride, &narrow)),
        weight_q8: 128,
    };
    compositor.compose(&fade).expect("compose the fade");
    let frame = compositor.read_back().expect("read back");
    let layers = fade.layers_in(FHD_WIDTH, FHD_HEIGHT);
    assert_matches_reference_in(&frame, &layers, size, "fade 50 % in 1920x1080");
    assert_opaque(&frame, "fade 50 % in 1920x1080");
}
