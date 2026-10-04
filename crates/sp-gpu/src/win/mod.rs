//! The Direct3D 11 compositor (Windows). Every decision it takes is a pure,
//! Linux-tested function of the crate (the adapter, the picture check, the
//! layers, the quad constants, the upload residency); this module only
//! calls Direct3D. It is out of the Linux mutation gate (`.cargo/mutants.toml`,
//! like `sp-decoder/src/video/`) and is proven by the WARP pixel pins
//! (`tests/warp.rs`) on `windows-latest`.

mod device;
mod pipeline;
mod textures;

use std::time::Instant;

use tracing::{debug, info};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::IDXGIResource;
use windows::core::Interface;

pub use device::adapters;

use crate::adapter::AdapterInfo;
use crate::composition::{CANVAS_HEIGHT, CANVAS_WIDTH, Composition};
use crate::error::GpuError;
use crate::quad::QuadConstants;
use crate::residency::{Resident, Upload, upload_for};
use crate::stats::ComposeStats;
use pipeline::{Pipeline, Quad};
use textures::{PlaneTextures, RenderTarget};

/// The error of a failed Direct3D / DXGI call, classified by its HRESULT.
fn failed(call: &'static str, error: &windows::core::Error) -> GpuError {
    GpuError::from_hresult(call, error.code().0 as u32)
}

/// Whole microseconds since `start`.
fn micros_since(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// The `SP-program-MAX` compositor: one Direct3D 11 device, the fixed
/// 3840×2160 BGRA render target, and two texture slots (a fade's outgoing
/// and incoming side). Not thread-safe by design: S2's `program-max` thread
/// owns it.
pub struct Compositor {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    adapter: AdapterInfo,
    pipeline: Pipeline,
    target: RenderTarget,
    slots: [Option<PlaneTextures>; 2],
    staging: Option<ID3D11Texture2D>,
}

impl Compositor {
    /// The compositor on the hardware adapter [`pick_adapter`](crate::pick_adapter)
    /// chooses (largest dedicated video memory, not software).
    /// [`GpuError::NoAdapter`] when there is none: it never falls back to
    /// WARP on its own.
    pub fn new() -> Result<Self, GpuError> {
        let ((device, context), adapter) = device::create_on_picked()?;
        Self::build(device, context, adapter, "hardware")
    }

    /// The compositor on WARP, Direct3D's CPU rasterizer: for tests and CI
    /// (`windows-latest` has no GPU).
    pub fn new_warp() -> Result<Self, GpuError> {
        let ((device, context), adapter) = device::create_warp()?;
        Self::build(device, context, adapter, "warp")
    }

    fn build(
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        adapter: AdapterInfo,
        driver: &'static str,
    ) -> Result<Self, GpuError> {
        let pipeline = Pipeline::new(&device)?;
        let target = RenderTarget::new(&device)?;
        let feature_level = unsafe { device.GetFeatureLevel() }.0;
        info!(
            driver,
            adapter = %adapter.name,
            vram_mb = adapter.dedicated_video_memory / (1024 * 1024),
            feature_level,
            width = CANVAS_WIDTH,
            height = CANVAS_HEIGHT,
            "sp-gpu: compositor ready (render target B8G8R8A8, shared)"
        );
        Ok(Self {
            device,
            context,
            adapter,
            pipeline,
            target,
            slots: [None, None],
            staging: None,
        })
    }

    /// The adapter the compositor runs on.
    pub fn adapter(&self) -> &AdapterInfo {
        &self.adapter
    }

    /// The Direct3D 11 device (S1b's Spout sender opens on it).
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// The 3840×2160 BGRA render target (what S1b sends).
    pub fn render_target(&self) -> &ID3D11Texture2D {
        &self.target.texture
    }

    /// The render target's legacy shared handle
    /// (`IDXGIResource::GetSharedHandle`; not an NT handle, never closed).
    pub fn shared_handle(&self) -> Result<HANDLE, GpuError> {
        let resource: IDXGIResource = self
            .target
            .texture
            .cast()
            .map_err(|e| failed("IDXGIResource", &e))?;
        unsafe { resource.GetSharedHandle() }.map_err(|e| failed("GetSharedHandle", &e))
    }

    /// Draw one boundary into the render target and wait until the GPU has
    /// finished it. Every picture is checked first: a picture that is not
    /// whole NV12 fails the call before anything is uploaded or drawn (the
    /// render target keeps the last frame). A [`GpuError::DeviceLost`] means
    /// the compositor must be rebuilt.
    pub fn compose(&mut self, composition: &Composition<'_>) -> Result<ComposeStats, GpuError> {
        let layers = composition.layers();
        let planes = layers
            .iter()
            .map(|layer| layer.picture.planes())
            .collect::<Result<Vec<_>, _>>()?;

        let upload_start = Instant::now();
        let mut uploads = 0;
        for (layer, planes) in layers.iter().zip(&planes) {
            let slot = &mut self.slots[layer.slot.index()];
            let upload = upload_for(slot.as_ref().map(|t| t.resident), &layer.picture);
            if upload == Upload::Skip {
                continue;
            }
            if upload == Upload::Create || slot.is_none() {
                debug!(
                    slot = layer.slot.index(),
                    width = layer.picture.width,
                    height = layer.picture.height,
                    "sp-gpu: new plane textures"
                );
                let resident = Resident::of(&layer.picture);
                *slot = Some(PlaneTextures::new(&self.device, planes, resident)?);
            }
            if let Some(textures) = slot.as_mut() {
                textures.write(&self.context, &layer.picture, planes);
            }
            uploads += 1;
        }
        let upload_us = micros_since(upload_start);

        let draw_start = Instant::now();
        let quads: Vec<Quad<'_>> = layers
            .iter()
            .filter_map(|layer| {
                self.slots[layer.slot.index()]
                    .as_ref()
                    .map(|textures| Quad {
                        constants: QuadConstants::new(layer.place, layer.weight),
                        textures,
                    })
            })
            .collect();
        self.pipeline.draw(&self.context, &self.target, &quads);
        self.pipeline.wait_until_done(&self.context)?;
        let draw_us = micros_since(draw_start);
        unsafe { self.device.GetDeviceRemovedReason() }
            .map_err(|e| failed("GetDeviceRemovedReason", &e))?;
        Ok(ComposeStats {
            upload_us,
            draw_us,
            uploads,
        })
    }

    /// The render target's pixels: 3840×2160 BGRA, row after row (for tests,
    /// and later the NDI readback, R3-3).
    pub fn read_back(&mut self) -> Result<Vec<u8>, GpuError> {
        let staging = match &self.staging {
            Some(staging) => staging.clone(),
            None => {
                let staging = textures::staging(&self.device)?;
                self.staging = Some(staging.clone());
                staging
            }
        };
        textures::read_back(&self.context, &self.target, &staging)
    }
}
