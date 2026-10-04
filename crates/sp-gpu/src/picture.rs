//! One NV12 picture as the compositor reads it, and the check that it is
//! whole before anything is uploaded (pure).

use sp_core::nv12::{nv12_chroma_row, nv12_len};

/// The largest picture side a Direct3D 11 texture takes
/// (`D3D11_REQ_TEXTURE2D_U_OR_V_DIMENSION`).
pub const MAX_PICTURE_SIDE: u32 = 16_384;

/// One NV12 picture (borrowed): a `width`×`height` luma plane of
/// `stride`-byte rows, then the interleaved UV plane, ⌈height/2⌉ rows of
/// the same stride, each ⌈width/2⌉ U/V pairs. The bytes of a row past its
/// pixels are never read.
#[derive(Debug, Clone, Copy)]
pub struct Nv12Picture<'a> {
    /// The caller's identity of this picture's CONTENT: a picture with the
    /// id already uploaded on its side is not uploaded again. Never reuse an
    /// id for other bytes — take it from a counter, never from an address
    /// (a freed buffer's address comes back).
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub data: &'a [u8],
}

/// Why a picture is refused (nothing is uploaded or drawn then).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PictureError {
    #[error("an empty picture ({width}x{height})")]
    Empty { width: u32, height: u32 },
    #[error("a {width}x{height} picture is over 16384 on a side")]
    TooLarge { width: u32, height: u32 },
    #[error("stride {stride} is shorter than a chroma row of {row} bytes")]
    Stride { stride: u32, row: usize },
    #[error("{len} bytes hold no whole NV12 picture ({need} bytes)")]
    Short { len: usize, need: usize },
}

/// One plane of a picture: its size in texels (a chroma texel is one U/V
/// pair) and its first byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plane {
    pub width: u32,
    pub height: u32,
    pub offset: usize,
}

/// Where a whole picture's planes are: what the upload reads, row by row,
/// `pitch` bytes apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Planes {
    pub luma: Plane,
    pub chroma: Plane,
    pub pitch: u32,
}

impl Nv12Picture<'_> {
    /// The planes of a whole picture, or why it is not one: no pixels, a
    /// side over [`MAX_PICTURE_SIDE`], a stride shorter than a chroma row
    /// (`sp_core::nv12::nv12_chroma_row`), or fewer bytes than the picture
    /// takes (`sp_core::nv12::nv12_len`) — the two sizes `sp-server`'s
    /// `nv12_whole` checks too.
    pub fn planes(&self) -> Result<Planes, PictureError> {
        let (width, height, stride) = (self.width, self.height, self.stride);
        if width == 0 || height == 0 {
            return Err(PictureError::Empty { width, height });
        }
        if width > MAX_PICTURE_SIDE || height > MAX_PICTURE_SIDE {
            return Err(PictureError::TooLarge { width, height });
        }
        let row = nv12_chroma_row(width);
        if (stride as usize) < row {
            return Err(PictureError::Stride { stride, row });
        }
        let need = nv12_len(stride, height);
        if self.data.len() < need {
            return Err(PictureError::Short {
                len: self.data.len(),
                need,
            });
        }
        Ok(Planes {
            luma: Plane {
                width,
                height,
                offset: 0,
            },
            chroma: Plane {
                width: width.div_ceil(2),
                height: height.div_ceil(2),
                offset: stride as usize * height as usize,
            },
            pitch: stride,
        })
    }
}

#[cfg(test)]
#[path = "picture_tests.rs"]
mod tests;
