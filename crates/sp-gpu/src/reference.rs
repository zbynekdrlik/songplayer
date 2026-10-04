//! The CPU model of what the compositor draws (pure): the expected BGRA of
//! one canvas pixel, for the WARP pixel pins (`tests/warp.rs`) and for any
//! later check of a GPU frame.
//!
//! It follows the GPU's own semantics step by step, not the CPU NV12 fit
//! (`sp-server`'s `nv12_mix`, which writes NV12 at the canvas's resolution):
//!
//! - **Coverage.** A quad covers the pixels whose centres lie inside its
//!   `aspect_fit` rectangle (its edges are on pixel edges, `quad::ndc_rect`).
//! - **Texture coordinates.** At pixel `x` the quad's u is `(x − x0 + ½) / w`
//!   (the rasterizer interpolates at the pixel centre), v alike.
//! - **Sampling.** Direct3D 11's bilinear filter on a texture of `size`
//!   texels: texel coordinate `u·size − ½`, the floor and the next texel
//!   weighted by the fraction, both clamped to the edge (CLAMP addressing).
//!   This is the pixel-centre rule of `program_transition::tap`. Luma is
//!   sampled on its own grid, the UV plane on its half-size grid, both at the
//!   same u, v, so chroma is interpolated at every output pixel.
//! - **Colour.** [`yuv_to_rgb`]: the f32 rows of
//!   [`BT709_LIMITED_TO_FULL`](crate::BT709_LIMITED_TO_FULL), saturated.
//! - **Blend.** The render target is cleared to black, then each layer is
//!   ADDED at its weight, in order, and stored in 8 bits between the draws:
//!   `c = unorm8(c/255 + rgb·w)`.
//!
//! What the GPU may do differently, and why the pins allow ±1 code value:
//! Direct3D 11 requires only 8 bits of sub-texel precision in the filter
//! weights (this model is exact), the float → UNORM rounding may differ by
//! up to 0.6 ULP from round-half-away, and the GPU computes in f32. With the
//! pins' smooth test pictures (≤ 12 codes per texel) the weights cost at
//! most ~0.15 of a code, so every channel is within 1 code of this model.

use crate::color::matrix_f32;
use crate::composition::Layer;
use crate::picture::Plane;

/// The expected BGRA at canvas pixel (`x`, `y`) once `layers` (a
/// composition's [`layers`](crate::Composition::layers)) are drawn over the
/// black canvas. Alpha is always 255: the clear writes it, the quads never
/// do.
pub fn pixel(layers: &[Layer<'_>], x: u32, y: u32) -> [u8; 4] {
    let mut rgb = [0u8; 3];
    for layer in layers {
        if let Some(sampled) = layer_rgb(layer, x, y) {
            for (channel, value) in rgb.iter_mut().zip(sampled) {
                *channel = unorm8(f64::from(*channel) / 255.0 + value * f64::from(layer.weight));
            }
        }
    }
    [rgb[2], rgb[1], rgb[0], 255]
}

/// The saturated RGB `layer`'s quad draws at canvas pixel (`x`, `y`) before
/// its weight, or `None` when the quad does not cover the pixel (or the
/// picture is not whole NV12: the compositor refuses one before drawing).
pub fn layer_rgb(layer: &Layer<'_>, x: u32, y: u32) -> Option<[f64; 3]> {
    let place = layer.place;
    let dx = x.checked_sub(place.off_x)?;
    let dy = y.checked_sub(place.off_y)?;
    if dx >= place.w || dy >= place.h {
        return None;
    }
    let u = (f64::from(dx) + 0.5) / f64::from(place.w);
    let v = (f64::from(dy) + 0.5) / f64::from(place.h);
    let picture = &layer.picture;
    let planes = picture.planes().ok()?;
    let (data, pitch) = (picture.data, planes.pitch);
    let luma = sample(data, planes.luma, pitch, 1, 0, u, v);
    let cb = sample(data, planes.chroma, pitch, 2, 0, u, v);
    let cr = sample(data, planes.chroma, pitch, 2, 1, u, v);
    Some(yuv_to_rgb(luma, cb, cr))
}

/// Direct3D 11's bilinear sample of one channel of `plane` at normalized
/// (`u`, `v`), as UNORM (code / 255). A texel is `channels` bytes, the one
/// read is byte `channel` of it (the UV plane: 2 bytes, U then V).
pub fn sample(
    data: &[u8],
    plane: Plane,
    pitch: u32,
    channels: usize,
    channel: usize,
    u: f64,
    v: f64,
) -> f64 {
    let (x0, x1, fx) = taps(u, plane.width);
    let (y0, y1, fy) = taps(v, plane.height);
    let texel = |x: usize, y: usize| {
        let at = plane.offset + y * pitch as usize + x * channels + channel;
        f64::from(data[at]) / 255.0
    };
    let top = texel(x0, y0) * (1.0 - fx) + texel(x1, y0) * fx;
    let bottom = texel(x0, y1) * (1.0 - fx) + texel(x1, y1) * fx;
    top * (1.0 - fy) + bottom * fy
}

/// The two texels the bilinear filter reads along an axis of `size` texels
/// at normalized coordinate `u`, and the weight of the second: texel
/// coordinate `t = u·size − ½`, the texels ⌊t⌋ and ⌊t⌋ + 1 clamped to
/// `[0, size − 1]`, the weight `t − ⌊t⌋` (unclamped: past an edge both
/// texels are the edge texel, so the weight no longer matters).
pub fn taps(u: f64, size: u32) -> (usize, usize, f64) {
    let t = u * f64::from(size) - 0.5;
    let floor = t.floor();
    let last = f64::from(size.saturating_sub(1));
    let first = floor.clamp(0.0, last) as usize;
    let second = (floor + 1.0).clamp(0.0, last) as usize;
    (first, second, t - floor)
}

/// The shader's colour conversion of sampled UNORM `y`, `u`, `v`: each
/// channel the dot product of its f32 row with (y, u, v, 1), saturated to
/// [0, 1].
pub fn yuv_to_rgb(y: f64, u: f64, v: f64) -> [f64; 3] {
    matrix_f32().map(|row| {
        let [a, b, c, d] = row.map(f64::from);
        (a * y + b * u + c * v + d).clamp(0.0, 1.0)
    })
}

/// A value stored in an 8-bit UNORM render target: clamped to [0, 1], times
/// 255, rounded to the nearest code.
pub fn unorm8(value: f64) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

#[cfg(test)]
#[path = "reference_tests.rs"]
mod tests;
