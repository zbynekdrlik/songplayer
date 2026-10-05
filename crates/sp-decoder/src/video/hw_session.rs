//! The Direct3D 11 device a `Hardware` reader hands to Media Foundation
//! (#223 S3b): `sp_gpu::VideoDevice` (the picked GPU, the video API,
//! multithread-protected) behind a DXGI device manager, set on the source
//! reader as `MF_SOURCE_READER_D3D_MANAGER`.
//!
//! Microsoft: "Use this attribute to provide a Direct3D device for any video
//! decoders loaded by the source reader ... Setting this attribute enables
//! the decoder to use DXVA." (MF_SOURCE_READER_D3D_MANAGER). The decoder MFT
//! opens its own handle on the manager's device; one session per reader, so
//! a lost device never touches another song.

use sp_gpu::VideoDevice;
use windows::Win32::Media::MediaFoundation::{IMFDXGIDeviceManager, MFCreateDXGIDeviceManager};

/// Which device a `Hardware` reader decodes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HwDevice {
    /// The adapter `sp_gpu::pick_adapter` chooses (production).
    Picked,
    /// WARP (tests and CI: `windows-latest` has no GPU).
    Warp,
}

/// A device manager reset to its own video device.
pub(crate) struct HwSession {
    manager: IMFDXGIDeviceManager,
    device: VideoDevice,
}

impl HwSession {
    /// The session on `which` device, or why there is none (the reader then
    /// opens the file in software).
    pub(crate) fn new(which: HwDevice) -> Result<Self, String> {
        let device = match which {
            HwDevice::Picked => VideoDevice::new(),
            HwDevice::Warp => VideoDevice::new_warp(),
        }
        .map_err(|e| format!("no video device: {e}"))?;
        let mut token: u32 = 0;
        let mut manager: Option<IMFDXGIDeviceManager> = None;
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }
            .map_err(|e| format!("MFCreateDXGIDeviceManager: {e}"))?;
        let manager = manager.ok_or("MFCreateDXGIDeviceManager gave no manager")?;
        unsafe { manager.ResetDevice(device.device(), token) }
            .map_err(|e| format!("IMFDXGIDeviceManager::ResetDevice: {e}"))?;
        Ok(Self { manager, device })
    }

    /// The manager the source reader is given.
    pub(crate) fn manager(&self) -> &IMFDXGIDeviceManager {
        &self.manager
    }

    /// The adapter's name (`NVIDIA GeForce RTX 3070 Ti` on the box).
    pub(crate) fn adapter_name(&self) -> &str {
        &self.device.adapter().name
    }
}
