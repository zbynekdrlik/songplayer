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
//! GPU decoder (`hw_decode::DecodePath::of_picture`: `D3D11_BIND_DECODER`).
//!
//! `read_texture_as_decoded_sample` (doc-hidden) runs this readback on a
//! texture the caller made, wrapped the way a decoder wraps its output: the
//! Windows CI test of the layout, where no GPU decoder exists.

use std::ffi::c_void;

use windows::Win32::Foundation::FALSE;
use windows::Win32::Graphics::Direct3D11::{D3D11_TEXTURE2D_DESC, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer2, IMFDXGIBuffer, IMFMediaBuffer, IMFSample, MF2DBuffer_LockFlags_Read,
    MFCreateDXGISurfaceBuffer, MFCreateSample,
};
use windows::core::Interface;

use crate::error::DecoderError;
use crate::hw_decode::{DecodePath, SurfaceLayout, mapped_from_scanline0};

/// A picture read back out of a DXGI surface.
#[derive(Debug)]
pub struct DxgiPicture {
    /// The packed NV12 bytes (a `frame_pool` buffer).
    pub data: Vec<u8>,
    /// Their row stride.
    pub stride: u32,
    /// Whether the texture was a decoder's output (`D3D11_BIND_DECODER`).
    pub path: DecodePath,
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
    /// `frame_pool` buffer, and say where it came from.
    pub(super) fn read(&self, width: u32, height: u32) -> Result<DxgiPicture, DecoderError> {
        read_picture(&self.buffer, &self.dxgi, width, height)
    }
}

/// The texture behind a DXGI buffer: its rows, its format, its bind flags.
fn surface_desc(dxgi: &IMFDXGIBuffer) -> Result<D3D11_TEXTURE2D_DESC, DecoderError> {
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
    Ok(desc)
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
        path: DecodePath::of_picture(Some(desc.BindFlags)),
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
    surface.read(width, height)
}
