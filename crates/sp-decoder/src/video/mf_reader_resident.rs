//! #279 P3a: the picture in a DXGI surface, a child module of
//! `mf_reader.rs` (split out for the 1000-line cap; as a child it reaches
//! the reader's private fields): on a `GpuResident` reader it is copied
//! into a slot of the pipeline's ring (`DxgiSurface::copy_into`, no bytes
//! read back), else read back into a `frame_pool` buffer (#223 S3b,
//! `DxgiSurface::read`). Either way at the size `hw_decode::visible_size`
//! cuts it to. A RETIRED producer's surfaces (`video_decode` = gpu stopped
//! taking effect while the song played) are read back from that picture
//! on, like a `Hardware` reader's: the GPU decoder goes on, no reopen, no
//! gap in the program.

use sp_core::nv12::nv12_chroma_row;
use sp_gpu::GpuFrame;
use windows::Win32::Media::MediaFoundation::IMFMediaType;

use super::{MediaFoundationVideoReader, VIDEO_STREAM, frame_size};
use crate::error::DecoderError;
use crate::hw_decode::{DecodePath, visible_size};
use crate::video::dxgi_frame::DxgiSurface;
use crate::video::hw_session::HwSession;

/// A surface's picture: its bytes (empty on the GPU), width, height,
/// stride, path, and the ring frame it lies in (GPU-resident).
pub(super) type SurfacePicture = (Vec<u8>, u32, u32, u32, DecodePath, Option<GpuFrame>);

impl MediaFoundationVideoReader {
    /// The picture in `surface`, at the size the current type gives
    /// (`visible_size`: never the decoder's padded rows): copied into the
    /// session's producer ring on a GPU-resident reader (its stride one
    /// chroma row, as a CPU picture of that size would have), else read
    /// back.
    pub(super) fn surface_picture(
        &self,
        surface: &DxgiSurface,
    ) -> Result<SurfacePicture, DecoderError> {
        let media_type: IMFMediaType = unsafe {
            self.reader
                .GetCurrentMediaType(VIDEO_STREAM)
                .map_err(|e| DecoderError::ReadSample(e.to_string()))?
        };
        let (width, height) = visible_size(frame_size(&media_type), self.reference);
        let scaled = self.scaled_from.is_some();
        match self.hw.as_ref().and_then(HwSession::producer) {
            Some(producer) if !producer.retired() => {
                let (frame, path) = surface.copy_into(producer, width, height, scaled)?;
                // A chroma row of a picture this wide fits a `u32` (≤ 16384).
                let stride = nv12_chroma_row(width) as u32;
                Ok((Vec::new(), width, height, stride, path, Some(frame)))
            }
            // No producer (`Hardware`), or a retired one: read back.
            _ => {
                let picture = surface.read(width, height, scaled)?;
                Ok((
                    picture.data,
                    width,
                    height,
                    picture.stride,
                    picture.path,
                    None,
                ))
            }
        }
    }
}
