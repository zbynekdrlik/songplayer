//! Helpers shared by the WARP tests (`tests/warp.rs`, `tests/spout.rs`,
//! #223 S1a/S1b): the compositor on WARP, the smooth NV12 pictures S1a's
//! tolerance needs (`reference.rs`), and the comparison with the CPU
//! reference. Only the Windows test binaries include it.

// Each test binary compiles its own copy and uses a different part of it.
#![allow(dead_code)]

use sp_gpu::{CANVAS_HEIGHT, CANVAS_WIDTH, Compositor, Layer, Nv12Picture, reference};

pub const W: u32 = CANVAS_WIDTH;
pub const H: u32 = CANVAS_HEIGHT;
pub const BLACK: [u8; 4] = [0, 0, 0, 255];

pub fn warp() -> Compositor {
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

/// Compare `frame` with the reference at `points(layers)`: each colour
/// channel within `reference::tolerance` codes, alpha exact. The worst
/// difference seen per tolerance (0, 1, 2) is printed (`--nocapture`).
pub fn assert_matches_reference(frame: &[u8], layers: &[Layer<'_>], what: &str) {
    assert_eq!(frame.len(), (W * H * 4) as usize, "{what}: frame size");
    let checked = points(layers);
    let mut worst = [0u8; 3];
    let mut over = Vec::new();
    for &(x, y) in &checked {
        let got = at(frame, x, y);
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
