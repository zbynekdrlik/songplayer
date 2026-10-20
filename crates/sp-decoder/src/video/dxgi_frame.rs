//! A picture the GPU decoded, copied out of its DXGI surface (#223 S3b).
//!
//! A DXVA decoder hands each picture over as a sample with one
//! `MFCreateDXGISurfaceBuffer` buffer over a slice of its `D3D11_BIND_DECODER`
//! texture array. That buffer is an `IMFDXGIBuffer` (the texture and the
//! slice) and an `IMF2DBuffer2`. `Lock2DSize` with a READ lock copies the
//! slice to the CPU and maps it (a read/write lock "can cause an extra copy
//! between CPU memory and GPU memory", Microsoft), giving scanline 0, the
//! pitch AND the mapping's bounds ("use these values to guard against buffer
//! overruns"). The mapped NV12 surface is Direct3D's layout: the texture's
//! `Height` luma rows `pitch` apart, then the UV plane.
//!
//! Every check is `hw_decode::SurfaceLayout::check` (pure, Linux-tested);
//! the copy packs the picture into a `frame_pool` buffer in the software
//! path's layout (`stride` = one chroma row, `height` luma rows, then
//! ⌈height/2⌉ UV rows), so the rest of the pipeline cannot tell the paths
//! apart. The texture's bind flags say whether the picture came out of the
//! GPU decoder (`hw_decode::DecodePath::of_picture`: `D3D11_BIND_DECODER`);
//! a picture of a file opened scaled (#282) lies in the video processor's
//! texture and counts as the GPU's (`DecodePath::of_surface`).
//!
//! `read_texture_as_decoded_sample` (doc-hidden) runs this readback on a
//! texture the caller made, wrapped the way a decoder wraps its output: the
//! Windows CI test of the layout, where no GPU decoder exists.
//!
//! #279 P3a: `DxgiSurface::picture` chooses by `hw_decode::SurfaceUse`.
//! On a GPU-resident reader whose producer is not retired the picture is
//! not read back: [`DxgiSurface::copy_into`] copies the slice the decoder
//! wrote (`GetResource` + `GetSubresourceIndex`) into a slot of the
//! pipeline's ring (`sp_gpu::ProducerGpu::copy_from`). A retired
//! producer's picture takes the readback above, as a `Hardware` reader's.
//! `copy_texture_as_decoded_sample` (doc-hidden) runs that same choice on
//! a slice of a texture array the caller made, the Windows CI test of
//! both.

use std::ffi::c_void;

use sp_core::nv12::nv12_chroma_row;
use sp_gpu::{GpuFrame, ProducerGpu};
use windows::Win32::Foundation::FALSE;
use windows::Win32::Graphics::Direct3D11::{D3D11_TEXTURE2D_DESC, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer2, IMFDXGIBuffer, IMFMediaBuffer, IMFSample, MF2DBuffer_LockFlags_Read,
    MFCreateDXGISurfaceBuffer, MFCreateSample,
};
use windows::core::Interface;

use crate::error::DecoderError;
use crate::hw_decode::{DecodePath, SurfaceLayout, SurfaceUse, mapped_from_scanline0};

/// A picture read back out of a DXGI surface.
#[derive(Debug)]
pub struct DxgiPicture {
    /// The packed NV12 bytes (a `frame_pool` buffer).
    pub data: Vec<u8>,
    /// Their row stride.
    pub stride: u32,
    /// Whether the texture was a decoder's output (`D3D11_BIND_DECODER`),
    /// or the video processor's of a scaled open (#282).
    pub path: DecodePath,
}

/// #279 P3a: a DXGI surface's picture as the reader hands it over
/// (`DxgiSurface::picture`).
#[derive(Debug)]
pub struct Surfaced {
    /// The packed NV12 bytes (read back); empty for a ring frame.
    pub data: Vec<u8>,
    /// Their row stride; for a ring frame one chroma row, as a CPU picture
    /// of that size would have.
    pub stride: u32,
    /// As [`DxgiPicture::path`].
    pub path: DecodePath,
    /// The ring slot it was copied into; `None` when read back.
    pub gpu: Option<GpuFrame>,
}

/// A sample's picture in a DXGI surface: its one buffer, as a media buffer
/// (for `IMF2DBuffer2`) and as a DXGI buffer (for the texture).
pub(super) struct DxgiSurface {
    buffer: IMFMediaBuffer,
    dxgi: IMFDXGIBuffer,
}

impl DxgiSurface {
    /// The sample's picture when its one buffer is a DXGI surface (the GPU
    /// decoded it), `None` when Media Foundation handed it over in system
    /// memory (decoded in software, also on the D3D path). A DXVA sample has
    /// exactly one buffer; any other count is read the software way.
    pub(super) fn of(sample: &IMFSample) -> Result<Option<Self>, DecoderError> {
        let count = unsafe { sample.GetBufferCount() }
            .map_err(|e| DecoderError::BufferLock(format!("GetBufferCount: {e}")))?;
        if count != 1 {
            return Ok(None);
        }
        let buffer = unsafe { sample.GetBufferByIndex(0) }
            .map_err(|e| DecoderError::BufferLock(format!("GetBufferByIndex: {e}")))?;
        let Ok(dxgi) = buffer.cast::<IMFDXGIBuffer>() else {
            return Ok(None);
        };
        Ok(Some(Self { buffer, dxgi }))
    }

    /// Copy the `width`×`height` picture out of the surface into a
    /// `frame_pool` buffer, and say where it came from (`scaled`: the file
    /// was opened scaled to the decode cap, #282).
    pub(super) fn read(
        &self,
        width: u32,
        height: u32,
        scaled: bool,
    ) -> Result<DxgiPicture, DecoderError> {
        read_picture(&self.buffer, &self.dxgi, width, height, scaled)
    }

    /// #279 P3a: the `width`×`height` picture as the reader hands it over:
    /// copied into `producer`'s ring when [`SurfaceUse::of`] says so (a
    /// GPU-resident reader whose producer is not retired), else read back.
    pub(super) fn picture(
        &self,
        producer: Option<&ProducerGpu>,
        width: u32,
        height: u32,
        scaled: bool,
    ) -> Result<Surfaced, DecoderError> {
        let retired = producer.is_some_and(|p| p.retired());
        match (SurfaceUse::of(producer.is_some(), retired), producer) {
            (SurfaceUse::Ring, Some(producer)) => {
                let (frame, path) = self.copy_into(producer, width, height, scaled)?;
                Ok(Surfaced {
                    data: Vec::new(),
                    // A chroma row of a picture this wide fits a `u32` (≤ 16384).
                    stride: nv12_chroma_row(width) as u32,
                    path,
                    gpu: Some(frame),
                })
            }
            _ => {
                let read = self.read(width, height, scaled)?;
                Ok(Surfaced {
                    data: read.data,
                    stride: read.stride,
                    path: read.path,
                    gpu: None,
                })
            }
        }
    }

    /// #279 P3a: copy the `width`×`height` picture into a slot of
    /// `producer`'s ring instead of reading it back (the texture and the
    /// slice the decoder wrote: `GetResource`, `GetSubresourceIndex`), and
    /// say where it came from (`scaled`: the file was opened scaled to the
    /// decode cap, #282). A refused picture, a ring full past its bound or
    /// dead, or a lost producer device is a decode error (`BufferLock`):
    /// the reader then reopens the file in software (`FallbackGate`).
    pub(super) fn copy_into(
        &self,
        producer: &ProducerGpu,
        width: u32,
        height: u32,
        scaled: bool,
    ) -> Result<(GpuFrame, DecodePath), DecoderError> {
        let (texture, desc) = surface_texture(&self.dxgi)?;
        let subresource = unsafe { self.dxgi.GetSubresourceIndex() }.map_err(|e| {
            DecoderError::BufferLock(format!("IMFDXGIBuffer::GetSubresourceIndex: {e}"))
        })?;
        let frame = producer
            .copy_from(&texture, subresource, (width, height))
            .map_err(|e| DecoderError::BufferLock(format!("GPU ring: {e}")))?;
        Ok((frame, DecodePath::of_surface(desc.BindFlags, scaled)))
    }
}

/// The texture behind a DXGI buffer: its rows, its format, its bind flags.
fn surface_desc(dxgi: &IMFDXGIBuffer) -> Result<D3D11_TEXTURE2D_DESC, DecoderError> {
    surface_texture(dxgi).map(|(_, desc)| desc)
}

/// The texture behind a DXGI buffer and its description.
fn surface_texture(
    dxgi: &IMFDXGIBuffer,
) -> Result<(ID3D11Texture2D, D3D11_TEXTURE2D_DESC), DecoderError> {
    let mut raw: *mut c_void = std::ptr::null_mut();
    unsafe { dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw) }
        .map_err(|e| DecoderError::BufferLock(format!("IMFDXGIBuffer::GetResource: {e}")))?;
    if raw.is_null() {
        return Err(DecoderError::BufferLock(
            "IMFDXGIBuffer::GetResource gave no texture".into(),
        ));
    }
    // SAFETY: GetResource returned an AddRef'd ID3D11Texture2D (the IID we
    // asked for); `from_raw` takes that reference and releases it on drop.
    let texture = unsafe { ID3D11Texture2D::from_raw(raw) };
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    Ok((texture, desc))
}

/// Unlocks a `Lock2DSize` on every path out.
struct Locked<'a>(&'a IMF2DBuffer2);

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        // An unlock failure leaves nothing to do; the next lock reports it.
        let _ = unsafe { self.0.Unlock2D() };
    }
}

/// [`DxgiSurface::read`].
fn read_picture(
    buffer: &IMFMediaBuffer,
    dxgi: &IMFDXGIBuffer,
    width: u32,
    height: u32,
    scaled: bool,
) -> Result<DxgiPicture, DecoderError> {
    let desc = surface_desc(dxgi)?;
    let two_d: IMF2DBuffer2 = buffer
        .cast()
        .map_err(|e| DecoderError::BufferLock(format!("IMF2DBuffer2: {e}")))?;
    let mut scanline0: *mut u8 = std::ptr::null_mut();
    let mut pitch: i32 = 0;
    let mut start: *mut u8 = std::ptr::null_mut();
    let mut len: u32 = 0;
    unsafe {
        two_d.Lock2DSize(
            MF2DBuffer_LockFlags_Read,
            &mut scanline0,
            &mut pitch,
            &mut start,
            &mut len,
        )
    }
    .map_err(|e| DecoderError::BufferLock(format!("Lock2DSize: {e}")))?;
    let _locked = Locked(&two_d);
    let pitch = usize::try_from(pitch).map_err(|_| {
        DecoderError::BufferLock(format!("a bottom-up NV12 surface (pitch {pitch})"))
    })?;
    let mapped = mapped_from_scanline0(start as usize, scanline0 as usize, len as usize)
        .ok_or_else(|| DecoderError::BufferLock("scanline 0 outside the mapping".into()))?;
    let layout = SurfaceLayout {
        pitch,
        surface_rows: desc.Height as usize,
        surface_cols: desc.Width as usize,
        width,
        height,
    };
    let copy = layout
        .check(desc.Format.0 as u32, mapped)
        .map_err(|e| DecoderError::BufferLock(format!("DXGI surface: {e}")))?;
    let mut picture =
        crate::frame_pool::try_take(copy.len).map_err(|e| DecoderError::FrameAlloc(e.bytes))?;
    // SAFETY: `scanline0 .. scanline0 + mapped` lies inside the locked
    // mapping (`Lock2DSize`'s bounds, `mapped_from_scanline0`), which stays
    // valid until `_locked` unlocks it after the copy.
    let src = unsafe { std::slice::from_raw_parts(scanline0.cast_const(), mapped) };
    copy.copy(src, &mut picture)
        .map_err(|e| DecoderError::BufferLock(format!("DXGI surface: {e}")))?;
    Ok(DxgiPicture {
        data: picture,
        stride: copy.stride,
        path: DecodePath::of_surface(desc.BindFlags, scaled),
    })
}

/// Read subresource 0 of `texture` back as a `width`×`height` picture,
/// wrapped the way a DXVA decoder wraps its output
/// (`MFCreateDXGISurfaceBuffer` in an `MFCreateSample` sample): the same
/// `DxgiSurface::of` + `read` a GPU-decoded sample takes. For the Windows CI
/// test of the readback (no GPU decoder there); not for production.
#[doc(hidden)]
pub fn read_texture_as_decoded_sample(
    texture: &ID3D11Texture2D,
    width: u32,
    height: u32,
) -> Result<DxgiPicture, DecoderError> {
    super::mf_reader::com_startup()?;
    let buffer = unsafe { MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, FALSE) }
        .map_err(|e| DecoderError::BufferLock(format!("MFCreateDXGISurfaceBuffer: {e}")))?;
    let sample = unsafe { MFCreateSample() }
        .map_err(|e| DecoderError::BufferLock(format!("MFCreateSample: {e}")))?;
    unsafe { sample.AddBuffer(&buffer) }
        .map_err(|e| DecoderError::BufferLock(format!("IMFSample::AddBuffer: {e}")))?;
    let surface = DxgiSurface::of(&sample)?.ok_or_else(|| {
        DecoderError::BufferLock("the wrapped texture is not a DXGI surface".into())
    })?;
    surface.read(width, height, false)
}

/// #279 P3a: hand subresource `subresource` of `texture` (a texture of
/// `producer`'s device: a decoder's array, made by the caller) over as a
/// `width`×`height` picture, wrapped the way a DXVA decoder wraps its
/// output (`MFCreateDXGISurfaceBuffer` of that subresource in an
/// `MFCreateSample` sample): the same `DxgiSurface::of` +
/// `DxgiSurface::picture` a GPU-resident reader's picture takes, so
/// copied into `producer`'s ring, or read back once `producer` is retired.
/// For the Windows CI test of the path (no GPU decoder there); not for
/// production.
#[doc(hidden)]
pub fn copy_texture_as_decoded_sample(
    producer: &ProducerGpu,
    texture: &ID3D11Texture2D,
    subresource: u32,
    width: u32,
    height: u32,
) -> Result<Surfaced, DecoderError> {
    super::mf_reader::com_startup()?;
    let buffer =
        unsafe { MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, subresource, FALSE) }
            .map_err(|e| DecoderError::BufferLock(format!("MFCreateDXGISurfaceBuffer: {e}")))?;
    let sample = unsafe { MFCreateSample() }
        .map_err(|e| DecoderError::BufferLock(format!("MFCreateSample: {e}")))?;
    unsafe { sample.AddBuffer(&buffer) }
        .map_err(|e| DecoderError::BufferLock(format!("IMFSample::AddBuffer: {e}")))?;
    let surface = DxgiSurface::of(&sample)?.ok_or_else(|| {
        DecoderError::BufferLock("the wrapped texture is not a DXGI surface".into())
    })?;
    surface.picture(Some(producer), width, height, false)
}
