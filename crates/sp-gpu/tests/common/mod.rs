//! Helpers shared by the WARP tests (`tests/warp.rs`, `tests/spout.rs`,
//! #223 S1a/S1b): the compositor on WARP, the smooth NV12 pictures S1a's
//! tolerance needs (`reference.rs`), and the comparison with the CPU
//! reference. Only the Windows test binaries include it.

// Each test binary compiles its own copy and uses a different part of it.
#![allow(dead_code)]

use sp_gpu::{
    CANVAS_HEIGHT, CANVAS_WIDTH, Compositor, FHD_HEIGHT, FHD_WIDTH, Layer, Nv12Picture, reference,
};

pub const W: u32 = CANVAS_WIDTH;
pub const H: u32 = CANVAS_HEIGHT;
pub const BLACK: [u8; 4] = [0, 0, 0, 255];

pub fn warp() -> Compositor {
    Compositor::new_warp().unwrap_or_else(|e| {
        panic!("WARP must build the compositor (device, shaders, shared target): {e}")
    })
}

/// #239: the `SP-program` sender's 1920×1080 compositor, on WARP.
pub fn warp_fhd() -> Compositor {
    Compositor::new_warp_with_size(FHD_WIDTH, FHD_HEIGHT)
        .unwrap_or_else(|e| panic!("WARP must build the 1920x1080 compositor: {e}"))
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
pub fn pattern(width: u32, height: u32, seed: u32) -> (u32, Vec<u8>) {
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

pub fn picture(id: u64, width: u32, height: u32, stride: u32, data: &[u8]) -> Nv12Picture<'_> {
    Nv12Picture {
        id,
        width,
        height,
        stride,
        data,
    }
}

pub fn at(frame: &[u8], x: u32, y: u32) -> [u8; 4] {
    at_in(frame, W, x, y)
}

/// The BGRA at (`x`, `y`) of a frame `width` pixels wide.
pub fn at_in(frame: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * width + x) * 4) as usize;
    [frame[i], frame[i + 1], frame[i + 2], frame[i + 3]]
}

/// The points a `w`×`h` frame is checked at: a 7×9 grid over the target,
/// every third pixel along each quad's edges (one pixel either side too),
/// and the corners.
fn points(layers: &[Layer<'_>], (w, h): (u32, u32)) -> Vec<(u32, u32)> {
    let mut points: Vec<(u32, u32)> = (0..h)
        .step_by(9)
        .flat_map(|y| (0..w).step_by(7).map(move |x| (x, y)))
        .collect();
    points.extend([(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)]);
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
        for y in edge(p.off_y, p.h).into_iter().filter(|&y| y < h) {
            points.extend((0..w).step_by(3).map(|x| (x, y)));
        }
        for x in edge(p.off_x, p.w).into_iter().filter(|&x| x < w) {
            points.extend((0..h).step_by(3).map(|y| (x, y)));
        }
    }
    points
}

/// Compare a 3840×2160 `frame` with the reference: [`assert_matches_reference_in`]
/// MAX's canvas.
pub fn assert_matches_reference(frame: &[u8], layers: &[Layer<'_>], what: &str) {
    assert_matches_reference_in(frame, layers, (W, H), what);
}

/// Compare a `size` `frame` with the reference at `points(layers, size)`:
/// each colour channel within `reference::tolerance` codes, alpha exact.
/// The worst difference seen per tolerance (0, 1, 2) is printed
/// (`--nocapture`).
pub fn assert_matches_reference_in(
    frame: &[u8],
    layers: &[Layer<'_>],
    size: (u32, u32),
    what: &str,
) {
    let (w, h) = size;
    assert_eq!(frame.len(), (w * h * 4) as usize, "{what}: frame size");
    let checked = points(layers, size);
    let mut worst = [0u8; 3];
    let mut over = Vec::new();
    for &(x, y) in &checked {
        let got = at_in(frame, w, x, y);
        let want = reference::pixel(layers, x, y);
        let tolerance = reference::tolerance(layers, x, y);
        let diff = got[..3]
            .iter()
            .zip(&want[..3])
            .map(|(g, w)| g.abs_diff(*w))
            .max()
            .unwrap_or(0);
        let class = usize::from(tolerance.min(2));
        worst[class] = worst[class].max(diff);
        if diff > tolerance || got[3] != want[3] {
            over.push(format!(
                "({x}, {y}): got {got:?}, want {want:?} within {tolerance}"
            ));
        }
    }
    println!("{what}: worst difference by tolerance 0/1/2: {worst:?}");
    assert!(
        over.is_empty(),
        "{what}: {} of {} points outside the tolerance (worst by tolerance 0/1/2: {worst:?}), first: {:?}",
        over.len(),
        checked.len(),
        &over[..over.len().min(10)]
    );
}
