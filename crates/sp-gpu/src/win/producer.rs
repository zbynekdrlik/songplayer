//! #279 P3a: a pipeline's PRODUCER device and its picture ring (glue).
//!
//! The GPU-resident path (epic #279, plan §5.1-§5.3; the P3a design
//! comment 6244390810) decodes each playlist on its own Direct3D 11
//! device: the adapter `pick_adapter` chooses (the compositor's), the
//! video API, multithread-protected (`VideoDevice`: Media Foundation's decoder MFT
//! drives it from its own threads). Each decoded picture, a slice of the
//! decoder's texture array on that device, is copied into a slot of the
//! pipeline's ring ([`PictureRing::copy_from`]) and travels on as a
//! [`GpuFrame`]: no readback, no upload. sp-server keeps one `ProducerGpu`
//! per pipeline across its songs (its lease, `playback::gpu_resident`);
//! sp-decoder's reader resets its DXGI device manager to
//! [`ProducerGpu::device`] and calls [`ProducerGpu::copy_from`] per picture.
//!
//! A device lost at a copy (`GetDeviceRemovedReason` read after every copy,
//! as the compositor reads it after every compose) kills the ring: this
//! copy and every later one fail, the reader goes on in software, and the
//! lease builds a new producer for a later song. Every decision of the ring
//! is `crate::ring_state`'s; this module only calls Direct3D.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};

use super::device::{self, DeviceUse};
use super::ring::PictureRing;
use super::video_device::VideoDevice;
use crate::adapter::AdapterInfo;
use crate::error::GpuError;
use crate::ring::{GpuFrame, RingCore, RingStats};
use crate::ring_state::RING_CAPACITY;

/// A pipeline's producer device and its ring of [`RING_CAPACITY`] slots.
/// `Send + Sync`: the device is thread-safe, and the ring's writes go
/// through one lock (one decode thread writes at a time: a pipeline opens
/// its songs one after another).
pub struct ProducerGpu {
    device: ID3D11Device,
    adapter: AdapterInfo,
    core: Arc<RingCore>,
    ring: Mutex<PictureRing>,
    /// [`retire`](Self::retire)d: its songs leave it at their next picture.
    retired: AtomicBool,
}

impl ProducerGpu {
    /// The producer on the adapter `pick_adapter` chooses, with the video
    /// API (`VideoDevice::new`): [`GpuError::NoAdapter`] when there is none
    /// (the pipeline then decodes in software).
    pub fn new() -> Result<Self, GpuError> {
        let video = VideoDevice::new()?;
        Self::on(video.device(), video.adapter().clone())
    }

    /// A producer on a WARP device of its own WITHOUT the video API (WARP
    /// refuses it on `windows-latest`, `video-decode.md`), for the CI tests
    /// of the copy path: a test writes its decoder-like textures on
    /// [`device`](Self::device). Not for production.
    #[doc(hidden)]
    pub fn new_warp() -> Result<Self, GpuError> {
        let ((device, _context), adapter) = device::create_warp(DeviceUse::Probe)?;
        Self::on(&device, adapter)
    }

    fn on(device: &ID3D11Device, adapter: AdapterInfo) -> Result<Self, GpuError> {
        let ring = PictureRing::new(device, RING_CAPACITY)?;
        Ok(Self {
            device: device.clone(),
            adapter,
            core: ring.core(),
            ring: Mutex::new(ring),
            retired: AtomicBool::new(false),
        })
    }

    /// The producer device (what the reader's DXGI device manager is reset
    /// to, and what the decoder decodes on).
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// The adapter the device runs on.
    pub fn adapter(&self) -> &AdapterInfo {
        &self.adapter
    }

    /// The ring's counters (no lock on the ring: a copy in progress never
    /// delays a status read).
    pub fn stats(&self) -> RingStats {
        self.core.stats()
    }

    /// Whether the ring is dead (a lost device, a slot's key abandoned):
    /// every later copy fails, and the lease builds a new producer. The
    /// device is asked too: Media Foundation's decoder may meet a removed
    /// device first (its read fails, the reader goes on in software and no
    /// copy runs), and the ring dies here then, so the pipeline's next song
    /// still builds a new producer.
    pub fn ring_dead(&self) -> bool {
        if self.core.is_dead() {
            return true;
        }
        match unsafe { self.device.GetDeviceRemovedReason() } {
            Ok(()) => false,
            Err(e) => {
                let hresult = e.code().0 as u32;
                self.core
                    .kill("the producer device was lost", Some(hresult));
                true
            }
        }
    }

    /// #279 P3a: stop its copies: the songs decoding on it go on in
    /// software from their next picture (the reader asks
    /// [`retired`](Self::retired) before each copy). Its ring and the frames
    /// in it stay valid: the compositor draws them to their last drop.
    /// sp-server retires every producer when `video_decode` = `gpu` stops
    /// taking effect.
    pub fn retire(&self) {
        self.retired.store(true, Ordering::Relaxed);
    }

    /// Whether it was [`retire`](Self::retire)d.
    pub fn retired(&self) -> bool {
        self.retired.load(Ordering::Relaxed)
    }

    /// Copy the top-left `visible` picture of subresource `subresource` of
    /// `source` (a texture of [`device`](Self::device): a slice of the
    /// decoder's texture array, or the video processor's output of a
    /// scaled open) into a ring slot ([`PictureRing::copy_from`]) and hand
    /// it over, timed into [`RingStats::copy_us_p99`] (the whole call: the
    /// ring's lock, a wait for a free slot, a new slot's texture, the key,
    /// the copy, its flush, the release). Then the device's
    /// removed reason: a lost device kills the ring and is
    /// [`GpuError::DeviceLost`], whatever the copy answered.
    pub fn copy_from(
        &self,
        source: &ID3D11Texture2D,
        subresource: u32,
        visible: (u32, u32),
    ) -> Result<GpuFrame, GpuError> {
        let start = Instant::now();
        let copied = self
            .ring
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .copy_from(source, subresource, visible);
        if let Err(e) = unsafe { self.device.GetDeviceRemovedReason() } {
            let hresult = e.code().0 as u32;
            self.core
                .kill("the producer device was lost", Some(hresult));
            return Err(GpuError::removed(hresult));
        }
        let frame = copied?;
        let us = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
        self.core.record_copy(us);
        Ok(frame)
    }
}

/// The adapter and the ring, never a device pointer.
impl fmt::Debug for ProducerGpu {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProducerGpu")
            .field("adapter", &self.adapter.name)
            .field("ring", &self.core.id())
            .field("dead", &self.core.is_dead())
            .field("retired", &self.retired())
            .finish()
    }
}
