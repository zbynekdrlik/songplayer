//! The Media Foundation half of a scaled open (#282): what the NV12 output
//! type of a file opened at the decode cap carries, the native type's facts
//! it copies, the read of the source reader's transform chain (the check
//! that the decoder's padding cannot reach the video processor), and the
//! tests' refusal hook. Every decision is `crate::hw_decode`'s
//! (Linux-tested); this file only calls Media Foundation. Split out of
//! `mf_reader.rs` for the 1000-line cap; the reader's own scaled half is
//! `mf_reader_scaled.rs`.
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
//!
//! #282 follow-up: a scaled open's facts ([`ScaledOpenFacts`]: each
//! transform of the chain, the native geometric aperture, the colour
//! descriptions, the decoder's output at the open and at the first picture)
//! are logged and kept for the tests, which print them to the CI log.

use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Media::MediaFoundation::{
    IMFMediaType, IMFSourceReader, IMFSourceReaderEx, IMFTransform, MF_E_INVALIDINDEX,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_GEOMETRIC_APERTURE, MF_MT_MINIMUM_DISPLAY_APERTURE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_YUV_MATRIX,
    MFT_CATEGORY_VIDEO_DECODER, MFT_CATEGORY_VIDEO_PROCESSOR,
};
use windows::core::{GUID, Interface};

use super::mf_reader::{VIDEO_STREAM, frame_size};
use crate::error::DecoderError;
use crate::hw_decode::{VideoArea, scaled_pixel_aspect, video_area};

/// A test's refusal of the next scaled open
/// (`MediaFoundationVideoReader::refuse_next_scaled_open_for_test`).
static REFUSE_NEXT: AtomicBool = AtomicBool::new(false);

/// How far the source reader's transform chain is searched.
const MAX_TRANSFORMS: u32 = 8;

/// A test's refusal of the next scaled open in this process, armed until a
/// scaled open uses it or this guard drops. A test that panics before its
/// open never leaves the refusal armed for the next test of its binary.
#[doc(hidden)]
#[must_use = "the refusal is disarmed when this guard drops"]
pub struct ScaledOpenRefusal(());

impl Drop for ScaledOpenRefusal {
    fn drop(&mut self) {
        REFUSE_NEXT.store(false, Ordering::SeqCst);
    }
}

/// Arm the refusal of the next scaled open in this process (tests only).
pub(super) fn refuse_next() -> ScaledOpenRefusal {
    REFUSE_NEXT.store(true, Ordering::SeqCst);
    ScaledOpenRefusal(())
}

/// Whether a test refused this scaled open (the refusal is used up).
pub(super) fn refused_by_test() -> bool {
    REFUSE_NEXT.swap(false, Ordering::SeqCst)
}

/// A media type's colour description, `None` where it states none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Colour {
    /// `MF_MT_YUV_MATRIX` (`MFVideoTransferMatrix`: 1 = BT.709, 2 = BT.601).
    pub matrix: Option<u32>,
    /// `MF_MT_VIDEO_NOMINAL_RANGE` (`MFNominalRange`: 1 = full, 2 = 16..235).
    pub range: Option<u32>,
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
    /// The native type's colour description, put on the output type.
    pub(super) colour: Colour,
    /// The native type's `MF_MT_FRAME_RATE` (packed): reported only when the
    /// processor's type has no rate, never put on the output type.
    pub(super) native_rate: Option<u64>,
    /// The native type's `MF_MT_GEOMETRIC_APERTURE` (logged only).
    pub(super) native_aperture: Option<VideoArea>,
}

impl ScaledType {
    /// The scaled type of a `from`-sized stream decoded at `to`, with what
    /// it keeps of the `native` type.
    pub(super) fn new(native: Option<&IMFMediaType>, from: (u32, u32), to: (u32, u32)) -> Self {
        ScaledType {
            from,
            size: to,
            pixel_aspect: scaled_pixel_aspect(from, native.and_then(pixel_aspect_of), to),
            colour: native.map(colour_of).unwrap_or_default(),
            native_rate: native.and_then(frame_rate_of),
            native_aperture: native.and_then(|t| area_of(t, &MF_MT_GEOMETRIC_APERTURE)),
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
            if let Some(matrix) = self.colour.matrix {
                media_type
                    .SetUINT32(&MF_MT_YUV_MATRIX, matrix)
                    .map_err(|e| refused("MF_MT_YUV_MATRIX", e))?;
            }
            if let Some(range) = self.colour.range {
                media_type
                    .SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, range)
                    .map_err(|e| refused("MF_MT_VIDEO_NOMINAL_RANGE", e))?;
            }
        }
        Ok(())
    }
}

/// What the decoder MFT in the source reader's chain hands the video
/// processor: its output type's frame size, display aperture and colour
/// description.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecoderOutput {
    pub size: (u32, u32),
    pub aperture: Option<VideoArea>,
    pub colour: Colour,
}

/// One transform of the source reader's chain, by its MFT category.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainStage {
    /// `MFT_CATEGORY_VIDEO_DECODER`.
    Decoder,
    /// `MFT_CATEGORY_VIDEO_PROCESSOR`: the processor that scales.
    VideoProcessor,
    /// Another category (its GUID), or the error that ended the walk.
    Other(String),
}

/// What a scaled open read of Media Foundation (#282 follow-up), logged at
/// the open and at its first picture.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScaledOpenFacts {
    /// The source reader's transforms in chain order.
    pub chain: Vec<ChainStage>,
    /// The native type's `MF_MT_GEOMETRIC_APERTURE`.
    pub native_aperture: Option<VideoArea>,
    /// The native type's colour description (put on the scaled type).
    pub native_colour: Colour,
    /// The negotiated scaled type's colour description (`None`: Media
    /// Foundation did not answer).
    pub out_colour: Option<Colour>,
    /// The decoder's output at the open; `Err`: why it could not be read.
    pub decoder: Result<DecoderOutput, String>,
    /// The decoder's output read again at the first picture (`None` before
    /// it, or when the open could not read the decoder).
    pub decoder_at_first_picture: Option<Result<DecoderOutput, String>>,
}

impl ScaledOpenFacts {
    /// Whether the chain holds a video processor (the transform that
    /// scales).
    pub fn has_video_processor(&self) -> bool {
        self.chain.contains(&ChainStage::VideoProcessor)
    }
}

/// The source reader's transform chain after the output type was set.
pub(super) struct Chain {
    /// Each transform's category, in order.
    pub(super) stages: Vec<ChainStage>,
    /// The first decoder's output type, or why it could not be read.
    pub(super) decoder: Result<DecoderOutput, String>,
    /// That decoder, kept for the first picture's re-read.
    pub(super) decoder_mft: Option<IMFTransform>,
}

/// `reader`'s transform chain: every transform up to the end of the chain
/// (`MF_E_INVALIDINDEX`) or [`MAX_TRANSFORMS`], and the first decoder's
/// output type. Any other error ends the walk; before the decoder was
/// found it is the decoder's error, as the chain could not be read.
pub(super) fn read_chain(reader: &IMFSourceReader) -> Chain {
    let mut chain = Chain {
        stages: Vec::new(),
        decoder: Err(format!(
            "no decoder in the chain's first {MAX_TRANSFORMS} transforms"
        )),
        decoder_mft: None,
    };
    let ex: IMFSourceReaderEx = match reader.cast() {
        Ok(ex) => ex,
        Err(e) => {
            chain.decoder = Err(format!("IMFSourceReaderEx: {e}"));
            return chain;
        }
    };
    let mut found = false;
    for index in 0..MAX_TRANSFORMS {
        let mut category = GUID::from_u128(0);
        let mut transform: Option<IMFTransform> = None;
        let got = unsafe {
            ex.GetTransformForStream(
                VIDEO_STREAM,
                index,
                Some(&mut category as *mut _),
                &mut transform,
            )
        };
        if let Err(e) = got {
            if e.code() != MF_E_INVALIDINDEX {
                let why = format!("GetTransformForStream {index}: {e}");
                chain.stages.push(ChainStage::Other(why.clone()));
                if !found {
                    chain.decoder = Err(why);
                }
            }
            break;
        }
        if category == MFT_CATEGORY_VIDEO_DECODER {
            chain.stages.push(ChainStage::Decoder);
            if !found {
                found = true;
                chain.decoder = match transform {
                    Some(decoder) => {
                        let output = decoder_output_of(&decoder);
                        chain.decoder_mft = Some(decoder);
                        output
                    }
                    None => Err("the decoder MFT is null".to_string()),
                };
            }
        } else if category == MFT_CATEGORY_VIDEO_PROCESSOR {
            chain.stages.push(ChainStage::VideoProcessor);
        } else {
            chain
                .stages
                .push(ChainStage::Other(format!("{category:?}")));
        }
    }
    chain
}

/// What `decoder` hands the next transform now: its output type's size,
/// display aperture and colour description.
pub(super) fn decoder_output_of(decoder: &IMFTransform) -> Result<DecoderOutput, String> {
    let output = unsafe { decoder.GetOutputCurrentType(0) }
        .map_err(|e| format!("GetOutputCurrentType: {e}"))?;
    Ok(DecoderOutput {
        size: frame_size(&output),
        aperture: area_of(&output, &MF_MT_MINIMUM_DISPLAY_APERTURE),
        colour: colour_of(&output),
    })
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

/// The colour description of `reader`'s negotiated output type, `None`
/// when Media Foundation does not answer.
pub(super) fn current_colour(reader: &IMFSourceReader) -> Option<Colour> {
    let current = unsafe { reader.GetCurrentMediaType(VIDEO_STREAM) }.ok()?;
    Some(colour_of(&current))
}

/// A media type's colour description.
fn colour_of(media_type: &IMFMediaType) -> Colour {
    Colour {
        matrix: uint32_of(media_type, &MF_MT_YUV_MATRIX),
        range: uint32_of(media_type, &MF_MT_VIDEO_NOMINAL_RANGE),
    }
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

/// A media type's `MFVideoArea` attribute `key` (an aperture), when it has
/// one.
fn area_of(media_type: &IMFMediaType, key: &GUID) -> Option<VideoArea> {
    let mut blob = [0_u8; 16];
    let mut len: u32 = 0;
    unsafe { media_type.GetBlob(key, &mut blob, Some(&mut len)) }.ok()?;
    video_area(blob.get(..len as usize)?)
}
