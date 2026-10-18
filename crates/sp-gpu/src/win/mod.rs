//! The Direct3D 11 compositor and its Spout sender (Windows), and the device
//! Media Foundation decodes video on (`VideoDevice`, #223 S3b). Every decision
//! they take is a pure, Linux-tested function of the crate (the adapter, the
//! picture check, the layers, the quad constants, the upload residency, the
//! targets and their draw order, the Spout name rule, registry formats and
//! shim codes, the sender's registration in `spout_state`); this module only
//! calls Direct3D, Win32 and the Spout shim (`spout_shim.cpp`). It is out of
//! the Linux mutation gate (`.cargo/mutants.toml`, like
//! `sp-decoder/src/video/`) and is proven on WARP on `windows-latest`: the
//! compositor by the pixel pins (`tests/warp.rs`), the sender by
//! `tests/spout.rs`. #279 P1a: the UYVY pass (`uyvy.rs`, its decisions the
//! pure `crate::uyvy`) by `tests/uyvy.rs`. #279 P2: the picture ring on a
//! producer device (`ring.rs`) and the compositor's ring slots
//! (`sides.rs`), their decisions the pure `crate::ring_state`, by
//! `tests/ring.rs`.

mod device;
mod pipeline;
mod probe;
mod producer;
mod receiver;
mod ring;
mod sides;
mod spout_lock;
mod spout_registry;
mod spout_sender;
mod textures;
mod uyvy;
mod vblank;
mod video_device;

use std::marker::PhantomData;
use std::time::Instant;

use tracing::{debug, info};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Query, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::IDXGIResource;
use windows::core::Interface;

pub use device::adapters;
pub use probe::{probe_gpu, probe_on_listed_adapter, probe_warp};
pub use producer::ProducerGpu;
pub use receiver::read_shared_texture;
pub use ring::PictureRing;
pub use spout_lock::{ListSentinel, mutex_holder, spout_list_holder};
pub use spout_registry::{spout_sender_info, spout_sender_names};
pub use spout_sender::SpoutSender;
pub use vblank::VblankTracker;
pub use video_device::VideoDevice;

use crate::adapter::AdapterInfo;
use crate::composition::{CANVAS_HEIGHT, CANVAS_WIDTH, Composition, Fit, MAX_FIT, Side, Target};
use crate::error::GpuError;
use crate::quad::QuadConstants;
use crate::residency::{Resident, Upload, swap_slots, upload_for};
use crate::stats::{ComposeStats, split_uyvy_part, target_draws_us};
use crate::targets::{TargetId, Targets};
use crate::uyvy::UYVY_STAGING;
use device::DeviceUse;
use pipeline::{Pipeline, Quad};
use sides::RingSlots;
use textures::{PlaneTextures, RenderTarget};
use uyvy::UyvyPass;

/// The error of a failed Direct3D / DXGI call, classified by its HRESULT.
fn failed(call: &'static str, error: &windows::core::Error) -> GpuError {
    GpuError::from_hresult(call, error.code().0 as u32)
}

/// A layer's slot with no textures after its upload step: a compositor bug,
/// reported instead of drawing the boundary without that layer.
const NO_TEXTURES: GpuError = GpuError::NoObject {
    call: "the slot's plane textures",
};

/// #251 step 2: a target id the compositor does not hold (never added, or
/// removed): the caller's bug, reported instead of reading another target.
const NO_TARGET: GpuError = GpuError::NoObject {
    call: "the compositor's target (no such id)",
};

/// #279 P1a: a UYVY readback with no UYVY pass (never enabled, disabled,
/// or its target removed): the caller's bug.
const NO_UYVY: GpuError = GpuError::NoObject {
    call: "the UYVY pass (not enabled)",
};

/// Whole microseconds since `start`.
fn micros_since(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// One render target of the compositor and what it keeps for it.
struct Drawn {
    /// Its size and the rule that places each picture in it (#267).
    spec: Target,
    render: RenderTarget,
    /// Ended right after the target's draw: the GPU has passed it once the
    /// target is drawn (#251 step 2: one per target, one flush, one wait).
    done: ID3D11Query,
    /// The readback's copy, made at the first readback of this target.
    staging: Option<ID3D11Texture2D>,
    /// Its own part of the last compose's draw, µs (`target_draws_us`).
    draw_us: Option<u64>,
}

impl Drawn {
    fn new(device: &ID3D11Device, spec: Target) -> Result<Self, GpuError> {
        Ok(Self {
            spec,
            render: RenderTarget::new(device, spec.width, spec.height)?,
            done: pipeline::query(device)?,
            staging: None,
            draw_us: None,
        })
    }
}

/// The `SP-program-MAX` compositor: one Direct3D 11 device, its PRIMARY
/// render target (the fixed 3840×2160 BGRA canvas for MAX), and two texture
/// slots (a fade's outgoing and incoming side). #251 step 2: it may hold
/// more targets ([`Compositor::add_target`], `SP-program`'s 1920×1080 in
/// production): one [`compose`](Compositor::compose) uploads each picture
/// ONCE and draws every target from those textures, then waits once. Each
/// target's size and (#267) the [`Fit`] that places each picture in it are
/// fixed for the target's life, the primary's for the compositor's. Neither
/// `Send` nor `Sync`: its immediate context is not thread-safe, and a
/// [`SpoutSender`] made on it drives the same context (#223 S1b), so both
/// stay on the thread that made them (S2's `program-max`).
pub struct Compositor {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    adapter: AdapterInfo,
    pipeline: Pipeline,
    targets: Targets<Drawn>,
    slots: [Option<PlaneTextures>; 2],
    /// #279 P2: the ring slots opened on this device (each once).
    ring_slots: RingSlots,
    /// #279 P1a: the UYVY pass of one target, while enabled.
    uyvy: Option<UyvyPass>,
    /// Ties the compositor to its thread (a raw pointer is neither `Send`
    /// nor `Sync`).
    thread_bound: PhantomData<*const ()>,
}

impl Compositor {
    /// MAX's compositor (3840×2160, #267 [`MAX_FIT`]) on the hardware
    /// adapter [`pick_adapter`](crate::pick_adapter) chooses (largest
    /// dedicated video memory, not software). [`GpuError::NoAdapter`] when
    /// there is none: it never falls back to WARP on its own.
    pub fn new() -> Result<Self, GpuError> {
        Self::with_size(CANVAS_WIDTH, CANVAS_HEIGHT, MAX_FIT)
    }

    /// #239: a compositor whose primary target is `width`×`height` and
    /// places each picture by `fit` (#267), on the adapter
    /// [`Compositor::new`] takes.
    pub fn with_size(width: u32, height: u32, fit: Fit) -> Result<Self, GpuError> {
        let ((device, context), adapter) = device::create_on_picked(DeviceUse::Compose)?;
        Self::build(device, context, adapter, "hardware", (width, height, fit))
    }

    /// MAX's compositor on WARP, Direct3D's CPU rasterizer: for tests and CI
    /// (`windows-latest` has no GPU).
    pub fn new_warp() -> Result<Self, GpuError> {
        Self::new_warp_with_size(CANVAS_WIDTH, CANVAS_HEIGHT, MAX_FIT)
    }

    /// #239: a compositor with a `width`×`height` primary target placing by
    /// `fit`, on WARP, for tests and CI. Not for production.
    #[doc(hidden)]
    pub fn new_warp_with_size(width: u32, height: u32, fit: Fit) -> Result<Self, GpuError> {
        let ((device, context), adapter) = device::create_warp(DeviceUse::Compose)?;
        Self::build(device, context, adapter, "warp", (width, height, fit))
    }

    /// The compositor on DXGI's adapter `index` (the order of
    /// [`adapters`](crate::adapters)), whatever `pick_adapter` would choose:
    /// the device path [`Compositor::new`] takes, testable on a CI box whose
    /// only adapter is the Basic Render Driver. An index past the list is
    /// [`GpuError::NoAdapter`]. Not for production.
    #[doc(hidden)]
    pub fn new_on_listed_adapter(index: usize) -> Result<Self, GpuError> {
        let ((device, context), adapter) = device::create_on_listed(index, DeviceUse::Compose)?;
        Self::build(
            device,
            context,
            adapter,
            "listed",
            (CANVAS_WIDTH, CANVAS_HEIGHT, MAX_FIT),
        )
    }

    fn build(
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        adapter: AdapterInfo,
        driver: &'static str,
        (width, height, fit): (u32, u32, Fit),
    ) -> Result<Self, GpuError> {
        let pipeline = Pipeline::new(&device)?;
        let primary = Drawn::new(&device, Target { width, height, fit })?;
        // D3D_FEATURE_LEVEL in hex, as the SDK writes it (0xb100 = 11.1).
        let feature_level = format!("{:#x}", unsafe { device.GetFeatureLevel() }.0);
        info!(
            driver,
            adapter = %adapter.name,
            vram_mb = adapter.dedicated_video_memory / (1024 * 1024),
            feature_level = %feature_level,
            width,
            height,
            fit = ?fit,
            "sp-gpu: compositor ready (render target B8G8R8A8, shared)"
        );
        Ok(Self {
            device,
            context,
            adapter,
            pipeline,
            targets: Targets::new(primary),
            slots: [None, None],
            ring_slots: RingSlots::default(),
            uyvy: None,
            thread_bound: PhantomData,
        })
    }

    /// #251 step 2: add a render target of `target`'s size, placing each
    /// picture by its rule: from the next [`compose`](Self::compose) on it
    /// is drawn after every target already there, from the same uploaded
    /// pictures. Its id names it to [`SpoutSender::for_target`],
    /// [`read_back_target`](Self::read_back_target) and
    /// [`remove_target`](Self::remove_target).
    pub fn add_target(&mut self, target: Target) -> Result<TargetId, GpuError> {
        let drawn = Drawn::new(&self.device, target)?;
        let id = self.targets.add(drawn);
        info!(
            width = target.width,
            height = target.height,
            fit = ?target.fit,
            "sp-gpu: render target added (drawn from the same uploads)"
        );
        Ok(id)
    }

    /// #251 step 2: stop drawing the target `id` (a sender made on it keeps
    /// its own reference to the texture until it is dropped). Whether it was
    /// there: the primary target is never removed. #279 P1a: a UYVY pass of
    /// that target goes with it.
    pub fn remove_target(&mut self, id: TargetId) -> bool {
        let removed = self.targets.remove(id).is_some();
        if removed {
            info!("sp-gpu: render target removed");
            if self.uyvy.take_if(|pass| pass.target == id).is_some() {
                info!("sp-gpu: UYVY pass removed with its target");
            }
        }
        removed
    }

    /// #279 P1a: convert the target `target` to UYVY in every later
    /// [`compose`](Self::compose), after the targets' draws: a pass into an
    /// `R8G8B8A8_UNORM` texture of half its width (one texel per pixel
    /// pair, `crate::uyvy`), copied into a ring of `UYVY_STAGING` staging
    /// textures before the compose's one wait, for
    /// [`read_uyvy`](Self::read_uyvy). It replaces a pass already enabled,
    /// and nothing is read before the next compose. A target of odd width
    /// is [`GpuError::UyvyOddWidth`], an id the compositor does not hold
    /// [`GpuError::NoObject`]; a refused or failed call leaves the
    /// compositor as it was.
    pub fn enable_uyvy(&mut self, target: TargetId) -> Result<(), GpuError> {
        let drawn = self.targets.get(target).ok_or(NO_TARGET)?;
        let size = (drawn.spec.width, drawn.spec.height);
        let pass = UyvyPass::new(&self.device, target, &drawn.render.texture, size)?;
        self.uyvy = Some(pass);
        info!(
            width = size.0,
            height = size.1,
            staging = UYVY_STAGING,
            "sp-gpu: UYVY pass enabled (read back without a wait)"
        );
        Ok(())
    }

    /// #279 P1a: stop the UYVY pass; whether one was enabled.
    pub fn disable_uyvy(&mut self) -> bool {
        let disabled = self.uyvy.take().is_some();
        if disabled {
            info!("sp-gpu: UYVY pass disabled");
        }
        disabled
    }

    /// #279 P1a: the UYVY of the last compose that finished, into `out`
    /// (cleared once mapped): `width × 2` bytes a row (U Y0 V Y1 per pixel
    /// pair), `height` rows, packed, of the target
    /// [`enable_uyvy`](Self::enable_uyvy) named. `Ok(false)` when no compose
    /// finished since the pass was enabled; on `Ok(false)` or an error `out`
    /// keeps what it held (never send it then). The map never waits: the
    /// compose already waited
    /// for its copy, so `DXGI_ERROR_WAS_STILL_DRAWING` is an error that names
    /// it ([`GpuError::Api`]). No pass enabled is [`GpuError::NoObject`].
    pub fn read_uyvy(&mut self, out: &mut Vec<u8>) -> Result<bool, GpuError> {
        let pass = self.uyvy.as_ref().ok_or(NO_UYVY)?;
        pass.read(&self.context, out)
    }

    /// The adapter the compositor runs on.
    pub fn adapter(&self) -> &AdapterInfo {
        &self.adapter
    }

    /// The primary render target's size, fixed for the compositor's life:
    /// 3840×2160 for MAX.
    pub fn size(&self) -> (u32, u32) {
        let spec = self.targets.primary().spec;
        (spec.width, spec.height)
    }

    /// #251 step 2: the size of the target `id`, if the compositor holds it.
    pub fn target_size(&self, id: TargetId) -> Option<(u32, u32)> {
        self.targets
            .get(id)
            .map(|drawn| (drawn.spec.width, drawn.spec.height))
    }

    /// #251 step 2: the target `id`'s own part of the last compose's draw,
    /// µs (from the end of the target drawn before it, or the first draw
    /// command, until the GPU had finished it); `None` before a compose drew
    /// it, or for a target the compositor does not hold.
    pub fn draw_us(&self, id: TargetId) -> Option<u64> {
        self.targets.get(id).and_then(|drawn| drawn.draw_us)
    }

    /// The Direct3D 11 device (the [`SpoutSender`] opens on it).
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// The primary BGRA render target (what [`SpoutSender::new`] sends).
    pub fn render_target(&self) -> &ID3D11Texture2D {
        &self.targets.primary().render.texture
    }

    /// #251 step 2: the BGRA render target `id` (what
    /// [`SpoutSender::for_target`] sends), if the compositor holds it.
    pub fn target_texture(&self, id: TargetId) -> Option<&ID3D11Texture2D> {
        self.targets.get(id).map(|drawn| &drawn.render.texture)
    }

    /// The primary render target's legacy shared handle
    /// (`IDXGIResource::GetSharedHandle`; not an NT handle, never closed).
    pub fn shared_handle(&self) -> Result<HANDLE, GpuError> {
        let resource: IDXGIResource = self
            .render_target()
            .cast()
            .map_err(|e| failed("IDXGIResource", &e))?;
        unsafe { resource.GetSharedHandle() }.map_err(|e| failed("GetSharedHandle", &e))
    }

    /// Draw one boundary into every render target and wait until the GPU
    /// has finished them. Every picture is checked first: a picture that is
    /// not whole NV12 fails the call before anything is uploaded or drawn
    /// (each target keeps the last frame). #251: the two slots first trade
    /// places when that saves uploads ([`swap_slots`]: a fade's incoming
    /// picture is the next fade's outgoing one). #251 step 2: each picture
    /// ([`Composition::uploads`]) is uploaded ONCE into its slot, then
    /// every target is drawn from those textures in draw order (the primary
    /// first), each placed by its own rule ([`Composition::layers_on`]); one
    /// flush and one wait for all of them, each target's own part kept for
    /// [`draw_us`](Self::draw_us). #279 P1a: with a UYVY pass enabled, the
    /// pass and its staging copy run after the targets, inside the same
    /// flush and wait (their part is `ComposeStats::uyvy_us`); the copy
    /// counts for [`read_uyvy`](Self::read_uyvy) only once the compose
    /// finished. #279 P2: a ring frame's side ([`Side::Gpu`]) is never
    /// uploaded: its slot is opened once on this device and held for the
    /// compose (acquired with key 1 and a 0 ms timeout before the draws,
    /// released after the wait), decided by `ring_state::slot_draw`: a
    /// busy slot, an abandoned one, a dead ring's and one that fails its
    /// acquire or open on a healthy compositor are drawn black and counted
    /// (`ComposeStats::km_acquire_timeouts` / `abandoned` /
    /// `open_failures`); a failure while this device is removed fails the
    /// compose. A [`GpuError::DeviceLost`] means the compositor must be
    /// rebuilt.
    pub fn compose(&mut self, composition: &Composition<'_>) -> Result<ComposeStats, GpuError> {
        let pictures = composition.uploads();
        let planes = pictures
            .iter()
            .map(|(_, picture)| picture.planes())
            .collect::<Result<Vec<_>, _>>()?;
        let mut stats = ComposeStats::default();
        let held = self
            .ring_slots
            .hold(&self.device, &composition.frames(), &mut stats)?;

        let upload_start = Instant::now();
        let resident = self
            .slots
            .each_ref()
            .map(|s| s.as_ref().map(|t| t.resident.id));
        let mut wanted = [None; 2];
        for (slot_of, picture) in &pictures {
            wanted[slot_of.index()] = Some(picture.id);
        }
        if swap_slots(resident, wanted) {
            self.slots.swap(0, 1);
        }
        for ((slot_of, picture), planes) in pictures.iter().zip(&planes) {
            let slot = &mut self.slots[slot_of.index()];
            let upload = upload_for(slot.as_ref().map(|t| t.resident), picture);
            if upload == Upload::Skip {
                continue;
            }
            if upload == Upload::Create {
                debug!(
                    slot = slot_of.index(),
                    width = picture.width,
                    height = picture.height,
                    "sp-gpu: new plane textures"
                );
                let resident = Resident::of(picture);
                *slot = Some(PlaneTextures::new(&self.device, planes, resident)?);
            }
            let textures = slot.as_mut().ok_or(NO_TEXTURES)?;
            textures.write(&self.context, picture, planes);
            stats.uploads += 1;
            stats.upload_bytes += planes.upload_bytes();
        }
        stats.upload_us = micros_since(upload_start);

        let draw_start = Instant::now();
        for (_, drawn) in self.targets.iter() {
            let spec = drawn.spec;
            let layers = composition.layers_on(spec);
            let mut quads = Vec::with_capacity(layers.len());
            for layer in &layers {
                let views = match layer.side {
                    Side::Cpu(_) => {
                        let textures =
                            self.slots[layer.slot.index()].as_ref().ok_or(NO_TEXTURES)?;
                        Some((&textures.luma_view, &textures.chroma_view))
                    }
                    Side::Gpu(frame) => held.views(frame),
                };
                // #279 P2: a ring slot not held this compose adds nothing
                // (black where it would be).
                let Some((luma, chroma)) = views else {
                    continue;
                };
                quads.push(Quad {
                    constants: QuadConstants::new(
                        layer.place,
                        layer.uv_rect,
                        layer.weight,
                        spec.width,
                        spec.height,
                    ),
                    luma,
                    chroma,
                });
            }
            self.pipeline.draw(&self.context, &drawn.render, &quads);
            unsafe { self.context.End(&drawn.done) };
        }
        if let Some(pass) = &self.uyvy {
            pass.draw(&self.context)?;
        }
        let mut queries: Vec<&ID3D11Query> = self.targets.iter().map(|(_, d)| &d.done).collect();
        let targets_drawn = queries.len();
        queries.extend(self.uyvy.as_ref().map(|pass| &pass.done));
        let done = pipeline::wait_until_each_done(&self.context, &queries)?;
        // The GPU has finished reading every held ring slot: give them back.
        drop(held);
        stats.draw_us = micros_since(draw_start);
        let parts = target_draws_us(draw_start, &done);
        let (parts, uyvy_us) = split_uyvy_part(parts, targets_drawn);
        for ((_, drawn), part) in self.targets.iter_mut().zip(parts) {
            drawn.draw_us = Some(part);
        }
        unsafe { self.device.GetDeviceRemovedReason() }
            .map_err(|e| GpuError::removed(e.code().0 as u32))?;
        if let Some(pass) = &mut self.uyvy {
            pass.ring.written();
        }
        stats.uyvy_us = uyvy_us;
        Ok(stats)
    }

    /// The primary render target's pixels: [`size`](Self::size) BGRA, row
    /// after row (for tests, and later the NDI readback, R3-3).
    pub fn read_back(&mut self) -> Result<Vec<u8>, GpuError> {
        self.read_back_target(TargetId::PRIMARY)
    }

    /// #251 step 2: the pixels of the target `id`, its size BGRA, row after
    /// row (for tests). [`GpuError::NoObject`] for a target the compositor
    /// does not hold.
    pub fn read_back_target(&mut self, id: TargetId) -> Result<Vec<u8>, GpuError> {
        let drawn = self.targets.get_mut(id).ok_or(NO_TARGET)?;
        let (width, height) = (drawn.spec.width, drawn.spec.height);
        let staging = match &drawn.staging {
            Some(staging) => staging.clone(),
            None => {
                let staging = textures::staging(&self.device, width, height)?;
                drawn.staging = Some(staging.clone());
                staging
            }
        };
        textures::read_back(
            &self.context,
            &drawn.render.texture,
            &staging,
            width,
            height,
        )
    }
}
