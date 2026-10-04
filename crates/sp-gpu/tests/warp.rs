//! WARP pixel pins of the `SP-program-MAX` compositor (#223 S1a, R3-2 "CI").
//!
//! Each test draws on WARP, Direct3D's CPU rasterizer (`windows-latest` has
//! no GPU), reads the render target back, and compares it with the CPU model
//! `sp_gpu::reference` within ±1 code value per channel (why ±1:
//! `reference.rs`). The pictures are smooth triangle waves (≤ 12 luma codes
//! per texel, ≤ 8 chroma codes per chroma texel), so the GPU's 8-bit filter
//! weights stay far inside that bound, and every row carries padding bytes
//! of 0xFF past its pixels: a stride bug would show them.
//!
//! Windows only. A WARP capability the compositor needs (the shared render
//! target included) that is missing FAILS here; nothing is skipped.

#![cfg(windows)]

use sp_gpu::{
    CANVAS_HEIGHT, CANVAS_WIDTH, Composition, Compositor, GpuError, Layer, Nv12Picture,
    PictureError, pick_adapter, reference,
};
use windows::Win32::Graphics::Direct3D11::{D3D11_RESOURCE_MISC_SHARED, D3D11_TEXTURE2D_DESC};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

const W: u32 = CANVAS_WIDTH;
const H: u32 = CANVAS_HEIGHT;
const BLACK: [u8; 4] = [0, 0, 0, 255];

fn warp() -> Compositor {
    Compositor::new_warp().unwrap_or_else(|e| {
        panic!("WARP must build the compositor (device, shaders, shared target): {e}")
    })
}

/// `lo..=hi` up and down: 1 per step of `t`, continuous.
fn triangle(t: u32, lo: u8, hi: u8) -> u8 {
    let span = u32::from(hi - lo);
    let phase = t % (2 * span);
    let up = if phase <= span {
        phase
    } else {
        2 * span - phase
    };
    lo + up as u8
}

/// A smooth `width`×`height` NV12 picture whose rows carry 64 padding bytes
/// of 0xFF; `seed` shifts its waves (two seeds = two different pictures).
fn pattern(width: u32, height: u32, seed: u32) -> (u32, Vec<u8>) {
    let stride = width + 64;
    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    let mut data = vec![0xFF_u8; (stride * (height + ch)) as usize];
    for y in 0..height {
        for x in 0..width {
            data[(y * stride + x) as usize] = triangle(12 * x + 5 * y + seed, 16, 235);
        }
    }
    let chroma = (stride * height) as usize;
    for y in 0..ch {
        for x in 0..cw {
            let at = chroma + (y * stride + 2 * x) as usize;
            data[at] = triangle(8 * x + 3 * y + 2 * seed + 40, 16, 240);
            data[at + 1] = triangle(6 * x + 7 * y + 3 * seed + 90, 16, 240);
        }
    }
    (stride, data)
}

fn picture(id: u64, width: u32, height: u32, stride: u32, data: &[u8]) -> Nv12Picture<'_> {
    Nv12Picture {
        id,
        width,
        height,
        stride,
        data,
    }
}

fn at(frame: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = ((y * W + x) * 4) as usize;
    [frame[i], frame[i + 1], frame[i + 2], frame[i + 3]]
}

/// The points a frame is checked at: a 7×9 grid over the canvas, every
/// third pixel along each quad's edges (one pixel either side too), and the
/// corners.
fn points(layers: &[Layer<'_>]) -> Vec<(u32, u32)> {
    let mut points: Vec<(u32, u32)> = (0..H)
        .step_by(9)
        .flat_map(|y| (0..W).step_by(7).map(move |x| (x, y)))
        .collect();
    points.extend([(0, 0), (W - 1, 0), (0, H - 1), (W - 1, H - 1)]);
    for layer in layers {
        let p = layer.place;
        let edge = |start: u32, size: u32| {
            [
                start.wrapping_sub(1),
                start,
                start + 1,
                start + size - 2,
                start + size - 1,
                start + size,
            ]
        };
        for y in edge(p.off_y, p.h).into_iter().filter(|&y| y < H) {
            points.extend((0..W).step_by(3).map(|x| (x, y)));
        }
        for x in edge(p.off_x, p.w).into_iter().filter(|&x| x < W) {
            points.extend((0..H).step_by(3).map(|y| (x, y)));
        }
    }
    points
}

/// Compare `frame` with the reference at `points(layers)`: every channel
/// within ±1.
fn assert_matches_reference(frame: &[u8], layers: &[Layer<'_>], what: &str) {
    assert_eq!(frame.len(), (W * H * 4) as usize, "{what}: frame size");
    let checked = points(layers);
    let mut worst = 0u8;
    let mut over = Vec::new();
    for &(x, y) in &checked {
        let got = at(frame, x, y);
        let want = reference::pixel(layers, x, y);
        let diff = got
            .iter()
            .zip(want)
            .map(|(g, w)| g.abs_diff(w))
            .max()
            .unwrap_or(0);
        worst = worst.max(diff);
        if diff > 1 {
            over.push(format!("({x}, {y}): got {got:?}, want {want:?}"));
        }
    }
    assert!(
        over.is_empty(),
        "{what}: {} of {} points differ by more than 1 (worst {worst}), first: {:?}",
        over.len(),
        checked.len(),
        &over[..over.len().min(10)]
    );
}

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
    assert!(
        compositor.adapter().is_software(),
        "WARP is the Basic Render Driver: {:?}",
        compositor.adapter()
    );
}

#[test]
fn the_adapter_list_has_the_basic_render_driver_and_never_picks_it() {
    let adapters = sp_gpu::adapters().expect("DXGI lists its adapters");
    assert!(
        adapters
            .iter()
            .any(|a| a.vendor_id == 0x1414 && a.device_id == 0x8c && a.is_software()),
        "DXGI always lists the Basic Render Driver: {adapters:?}"
    );
    if let Some(picked) = pick_adapter(&adapters) {
        assert!(!adapters[picked].is_software(), "{adapters:?}");
    }
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
