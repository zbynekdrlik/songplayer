//! The NV12 layout arithmetic every reader of a picture shares (pure,
//! WASM-safe): a `height`-row luma plane of `stride`-byte rows, then the
//! interleaved UV plane of ⌈height/2⌉ rows of the same stride, each holding
//! ⌈width/2⌉ U/V pairs.
//!
//! #223 S1a: `sp-server`'s CPU program fit (`program_transition::nv12_whole`)
//! and the `SP-program-MAX` GPU compositor (`sp-gpu`'s
//! `Nv12Picture::planes`) decide "is this buffer a whole picture" by these
//! two numbers, so they never disagree.

/// The bytes of one chroma row of a `width`-pixel picture: ⌈width/2⌉ U/V
/// pairs. A stride shorter than this cannot hold the picture. Saturates at
/// `usize::MAX` (a 32-bit target), so a check against it never wraps.
pub fn nv12_chroma_row(width: u32) -> usize {
    (width as usize).div_ceil(2).saturating_mul(2)
}

/// The bytes a whole `height`-row picture of `stride`-byte rows takes: the
/// luma plane, then ⌈height/2⌉ chroma rows. Saturates at `usize::MAX`, so
/// "the buffer holds `nv12_len` bytes" can never pass on a wrapped product
/// (the GPU upload reads the planes in `unsafe` code on this check).
pub fn nv12_len(stride: u32, height: u32) -> usize {
    let (stride, height) = (stride as usize, height as usize);
    stride.saturating_mul(height.saturating_add(height.div_ceil(2)))
}

#[cfg(test)]
#[path = "nv12_tests.rs"]
mod tests;
