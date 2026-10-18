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
//!
//! #279 P3a: a `GpuResident` reader's session is reset to its pipeline's
//! producer device instead (`sp_gpu::ProducerGpu`, kept across the
//! pipeline's songs), so the decoder's pictures lie on the device whose ring
//! they are copied into; the manager stays per reader.

use std::sync::Arc;

use sp_gpu::{GpuError, ProducerGpu, VideoDevice};
use windows::Win32::Media::MediaFoundation::{IMFDXGIDeviceManager, MFCreateDXGIDeviceManager};

/// Which device a reader on the D3D path decodes on.
#[derive(Debug, Clone)]
pub(crate) enum HwDevice {
    /// The adapter `sp_gpu::pick_adapter` chooses (production `Hardware`).
    Picked,
    /// WARP (tests and CI: `windows-latest` has no GPU, and its WARP refuses
    /// the video device, so this is the refused-device open fall back).
    Warp,
    /// #279 P3a: a pipeline's producer device (`GpuResident`).
    Producer(Arc<ProducerGpu>),
}

/// The device a session's manager is reset to.
enum SessionDevice {
    /// The session's own (`Hardware`): one per reader.
    Own(VideoDevice),
    /// #279 P3a: the pipeline's producer, shared with its ring.
    Producer(Arc<ProducerGpu>),
}

/// A device manager reset to its video device.
pub(crate) struct HwSession {
    manager: IMFDXGIDeviceManager,
    device: SessionDevice,
}

/// Why a session has no device (the exact text the WARP test pins).
fn no_device(e: GpuError) -> String {
    format!("no video device: {e}")
}

impl HwSession {
    /// The session on `which` device, or why there is none (the reader then
    /// opens the file in software). A producer whose ring is dead (its
    /// device was lost) is no device.
    pub(crate) fn new(which: HwDevice) -> Result<Self, String> {
        let device = match which {
            HwDevice::Picked => SessionDevice::Own(VideoDevice::new().map_err(no_device)?),
            HwDevice::Warp => SessionDevice::Own(VideoDevice::new_warp().map_err(no_device)?),
            HwDevice::Producer(producer) if producer.ring_dead() => {
                return Err("the producer's ring is dead (its device was lost)".into());
            }
            HwDevice::Producer(producer) if producer.retired() => {
                return Err(
                    "the producer was retired (video_decode=gpu no longer takes effect)".into(),
                );
            }
            HwDevice::Producer(producer) => SessionDevice::Producer(producer),
        };
        let d3d = match &device {
            SessionDevice::Own(video) => video.device(),
            SessionDevice::Producer(producer) => producer.device(),
        };
        let mut token: u32 = 0;
        let mut manager: Option<IMFDXGIDeviceManager> = None;
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }
            .map_err(|e| format!("MFCreateDXGIDeviceManager: {e}"))?;
        let manager = manager.ok_or("MFCreateDXGIDeviceManager gave no manager")?;
        unsafe { manager.ResetDevice(d3d, token) }
            .map_err(|e| format!("IMFDXGIDeviceManager::ResetDevice: {e}"))?;
        Ok(Self { manager, device })
    }

    /// The manager the source reader is given.
    pub(crate) fn manager(&self) -> &IMFDXGIDeviceManager {
        &self.manager
    }

    /// The adapter's name (`NVIDIA GeForce RTX 3070 Ti` on the box).
    pub(crate) fn adapter_name(&self) -> &str {
        match &self.device {
            SessionDevice::Own(video) => &video.adapter().name,
            SessionDevice::Producer(producer) => &producer.adapter().name,
        }
    }

    /// #279 P3a: the producer whose ring a GPU-decoded picture is copied
    /// into; `None` for a `Hardware` session (it reads its pictures back).
    pub(crate) fn producer(&self) -> Option<&ProducerGpu> {
        match &self.device {
            SessionDevice::Own(_) => None,
            SessionDevice::Producer(producer) => Some(producer.as_ref()),
        }
    }
}
