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
/// pairs. A stride shorter than this cannot hold the picture.
pub fn nv12_chroma_row(width: u32) -> usize {
    2 * (width as usize).div_ceil(2)
}

/// The bytes a whole `height`-row picture of `stride`-byte rows takes: the
/// luma plane, then ⌈height/2⌉ chroma rows.
pub fn nv12_len(stride: u32, height: u32) -> usize {
    let (stride, height) = (stride as usize, height as usize);
    stride * (height + height.div_ceil(2))
}

#[cfg(test)]
#[path = "nv12_tests.rs"]
mod tests;
