//! The Direct3D 11 device Media Foundation decodes video on (#223 S3b).
//!
//! sp-decoder's `MediaFoundationVideoReader` in `Hardware` mode hands this
//! device to Media Foundation through a DXGI device manager
//! (`MF_SOURCE_READER_D3D_MANAGER`), so the decoder MFT decodes on the GPU
//! (DXVA; NVDEC on the box's RTX 3070 Ti). It runs on the adapter the
//! compositor runs on ([`pick_adapter`](crate::pick_adapter): the largest
//! dedicated video memory that is not software), never on WARP on its own:
//! no such adapter is [`GpuError::NoAdapter`], and the reader decodes that
//! file in software.
//!
//! Two things differ from the compositor's device:
//!
//! - it is created with `D3D11_CREATE_DEVICE_VIDEO_SUPPORT` (the Direct3D 11
//!   video API that DXVA decoding uses; [`DeviceUse::VideoDecode`]);
//! - it is multithread-protected (`ID3D11Multithread::SetMultithreadProtected`):
//!   Media Foundation drives it from its own worker threads, and Microsoft's
//!   "Supporting Direct3D 11 Video Decoding in Media Foundation" recommends
//!   the protection against deadlocks in `GetDecoderBuffer` /
//!   `ReleaseDecoderBuffer`.

use windows::Win32::Foundation::TRUE;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Multithread};
use windows::core::Interface;

use super::device::{self, DeviceUse};
use super::failed;
use crate::adapter::AdapterInfo;
use crate::error::GpuError;

/// A multithread-protected Direct3D 11 device with the video API, and the
/// adapter it runs on (read back from the device, like the compositor's).
pub struct VideoDevice {
    device: ID3D11Device,
    adapter: AdapterInfo,
}

impl VideoDevice {
    /// The device on the adapter [`pick_adapter`](crate::pick_adapter)
    /// chooses. [`GpuError::NoAdapter`] when there is none: it never falls
    /// back to WARP on its own (a CPU rasterizer decodes nothing faster than
    /// Media Foundation's software decoder).
    pub fn new() -> Result<Self, GpuError> {
        let ((device, _context), adapter) = device::create_on_picked(DeviceUse::VideoDecode)?;
        Self::protected(device, adapter)
    }

    /// The device on WARP: for tests and CI (`windows-latest` has no GPU).
    /// WARP accepts `D3D11_CREATE_DEVICE_VIDEO_SUPPORT`; it has no decoder
    /// profiles, so Media Foundation's decoder decodes in software on it.
    #[doc(hidden)]
    pub fn new_warp() -> Result<Self, GpuError> {
        let ((device, _context), adapter) = device::create_warp(DeviceUse::VideoDecode)?;
        Self::protected(device, adapter)
    }

    /// The device on DXGI's adapter `index` ([`adapters`](crate::adapters)'
    /// order), whatever `pick_adapter` would choose: [`VideoDevice::new`]'s
    /// explicit-adapter path, testable on a CI box whose only adapter is the
    /// Basic Render Driver. Not for production.
    #[doc(hidden)]
    pub fn new_on_listed_adapter(index: usize) -> Result<Self, GpuError> {
        let ((device, _context), adapter) =
            device::create_on_listed(index, DeviceUse::VideoDecode)?;
        Self::protected(device, adapter)
    }

    fn protected(device: ID3D11Device, adapter: AdapterInfo) -> Result<Self, GpuError> {
        let multithread: ID3D11Multithread =
            device.cast().map_err(|e| failed("ID3D11Multithread", &e))?;
        // Returns whether the protection was on before; it is on after.
        let _was_on = unsafe { multithread.SetMultithreadProtected(TRUE) };
        Ok(Self { device, adapter })
    }

    /// The Direct3D 11 device (what the DXGI device manager is reset to).
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// The adapter the device runs on.
    pub fn adapter(&self) -> &AdapterInfo {
        &self.adapter
    }
}
