//! The Media Foundation half of a scaled open (#282): what the NV12 output
//! type of a file opened at the decode cap carries, the native type's facts
//! it copies, the check that the decoder's padding cannot reach the video
//! processor, and the tests' refusal hook. Every decision is
//! `crate::hw_decode`'s (Linux-tested); this file only calls Media
//! Foundation. Split out of `mf_reader.rs` for the 1000-line cap.
//!
//! The output type (`apply`) carries the scaled size, the pixel aspect that
//! keeps the native display aspect (`hw_decode::scaled_pixel_aspect`), and
//! the native type's YUV matrix and nominal range when it has them (so the
//! video processor converts no colour: a size-based default, e.g. BT.601
//! for an SD output, would shift the hues the compositor reads as BT.709).
//! It carries NO frame rate (review round 2): a rate of ours could start
//! the processor's frame-rate conversion when the decoder reports another;
//! the native rate is only what the reader reports when the processor's
//! type has none (`hw_decode::scaled_frame_rate`).

use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Media::MediaFoundation::{
    IMFMediaType, IMFSourceReader, IMFSourceReaderEx, IMFTransform, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_YUV_MATRIX, MFT_CATEGORY_VIDEO_DECODER,
};
use windows::core::{GUID, Interface};

use super::mf_reader::{VIDEO_STREAM, frame_size};
use crate::error::DecoderError;
use crate::hw_decode::{VideoArea, scaled_pixel_aspect, video_area};

/// A test's refusal of the next scaled open
/// (`MediaFoundationVideoReader::refuse_next_scaled_open_for_test`).
static REFUSE_NEXT: AtomicBool = AtomicBool::new(false);

/// How far the source reader's transform chain is searched for its decoder.
const MAX_TRANSFORMS: u32 = 8;

/// Arm the refusal of the next scaled open in this process (tests only).
pub(super) fn refuse_next() {
    REFUSE_NEXT.store(true, Ordering::SeqCst);
}

/// Whether a test refused this scaled open (the refusal is used up).
pub(super) fn refused_by_test() -> bool {
    REFUSE_NEXT.swap(false, Ordering::SeqCst)
}

/// The NV12 output type of a scaled open, and the native facts it keeps.
#[derive(Debug, Clone, Copy)]
pub(super) struct ScaledType {
    /// The stream's own size.
    pub(super) from: (u32, u32),
    /// `hw_decode::scaled_size`.
    pub(super) size: (u32, u32),
    /// `hw_decode::scaled_pixel_aspect`: the native display aspect, kept
    /// exactly.
    pub(super) pixel_aspect: Option<(u32, u32)>,
    /// The native type's `MF_MT_YUV_MATRIX` / `MF_MT_VIDEO_NOMINAL_RANGE`.
    yuv_matrix: Option<u32>,
    nominal_range: Option<u32>,
    /// The native type's `MF_MT_FRAME_RATE` (packed): reported only when the
    /// processor's type has no rate, never put on the output type.
    pub(super) native_rate: Option<u64>,
}

impl ScaledType {
    /// The scaled type of a `from`-sized stream decoded at `to`, with what
    /// it keeps of the `native` type.
    pub(super) fn new(native: Option<&IMFMediaType>, from: (u32, u32), to: (u32, u32)) -> Self {
        ScaledType {
            from,
            size: to,
            pixel_aspect: scaled_pixel_aspect(from, native.and_then(pixel_aspect_of), to),
            yuv_matrix: native.and_then(|t| uint32_of(t, &MF_MT_YUV_MATRIX)),
            nominal_range: native.and_then(|t| uint32_of(t, &MF_MT_VIDEO_NOMINAL_RANGE)),
            native_rate: native.and_then(frame_rate_of),
        }
    }

    /// Put the scaled size, the pixel aspect and the native colour
    /// description on the NV12 `media_type`.
    pub(super) fn apply(&self, media_type: &IMFMediaType) -> Result<(), DecoderError> {
        let packed = |(a, b): (u32, u32)| (u64::from(a) << 32) | u64::from(b);
        let refused = |what: &str, e: windows::core::Error| {
            DecoderError::NoStream(format!("the scaled type's {what}: {e}"))
        };
        unsafe {
            media_type
                .SetUINT64(&MF_MT_FRAME_SIZE, packed(self.size))
                .map_err(|e| refused("MF_MT_FRAME_SIZE", e))?;
            if let Some(ratio) = self.pixel_aspect {
                media_type
                    .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, packed(ratio))
                    .map_err(|e| refused("MF_MT_PIXEL_ASPECT_RATIO", e))?;
            }
            if let Some(matrix) = self.yuv_matrix {
                media_type
                    .SetUINT32(&MF_MT_YUV_MATRIX, matrix)
                    .map_err(|e| refused("MF_MT_YUV_MATRIX", e))?;
            }
            if let Some(range) = self.nominal_range {
                media_type
                    .SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, range)
                    .map_err(|e| refused("MF_MT_VIDEO_NOMINAL_RANGE", e))?;
            }
        }
        Ok(())
    }
}

/// What the decoder MFT in the source reader's chain hands the video
/// processor: its output type's frame size and display aperture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DecoderOutput {
    pub(super) size: (u32, u32),
    pub(super) aperture: Option<VideoArea>,
}

/// The decoder's output type in `reader`'s transform chain (after the
/// output type was set), `None` when the chain cannot be read.
pub(super) fn decoder_output(reader: &IMFSourceReader) -> Option<DecoderOutput> {
    let chain: IMFSourceReaderEx = reader.cast().ok()?;
    for index in 0..MAX_TRANSFORMS {
        let mut category = GUID::from_u128(0);
        let mut transform: Option<IMFTransform> = None;
        unsafe {
            chain.GetTransformForStream(
                VIDEO_STREAM,
                index,
                Some(&mut category as *mut _),
                &mut transform,
            )
        }
        .ok()?;
        if category == MFT_CATEGORY_VIDEO_DECODER {
            let output = unsafe { transform?.GetOutputCurrentType(0) }.ok()?;
            return Some(DecoderOutput {
                size: frame_size(&output),
                aperture: aperture_of(&output),
            });
        }
    }
    None
}

/// The native (compressed) media type, `None` when MF does not answer.
pub(super) fn native_media_type(reader: &IMFSourceReader) -> Option<IMFMediaType> {
    unsafe { reader.GetNativeMediaType(VIDEO_STREAM, 0) }.ok()
}

/// The stream's own size: the native type's `MF_MT_FRAME_SIZE`, `None`
/// when it reports none.
pub(super) fn known_frame_size(native: &IMFMediaType) -> Option<(u32, u32)> {
    let size = frame_size(native);
    (size != (0, 0)).then_some(size)
}

/// A media type's `MF_MT_FRAME_RATE` (packed), when it has one.
pub(super) fn frame_rate_of(media_type: &IMFMediaType) -> Option<u64> {
    unsafe { media_type.GetUINT64(&MF_MT_FRAME_RATE) }.ok()
}

/// A media type's `MF_MT_PIXEL_ASPECT_RATIO`, when it has one.
fn pixel_aspect_of(media_type: &IMFMediaType) -> Option<(u32, u32)> {
    let packed = unsafe { media_type.GetUINT64(&MF_MT_PIXEL_ASPECT_RATIO) }.ok()?;
    Some(((packed >> 32) as u32, packed as u32))
}

/// A media type's UINT32 attribute `key`, when it has one.
fn uint32_of(media_type: &IMFMediaType, key: &GUID) -> Option<u32> {
    unsafe { media_type.GetUINT32(key) }.ok()
}

/// A media type's `MF_MT_MINIMUM_DISPLAY_APERTURE`, when it has one.
fn aperture_of(media_type: &IMFMediaType) -> Option<VideoArea> {
    let mut blob = [0_u8; 16];
    let mut len: u32 = 0;
    unsafe { media_type.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, &mut blob, Some(&mut len)) }
        .ok()?;
    video_area(blob.get(..len as usize)?)
}
