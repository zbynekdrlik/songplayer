//! #279 P2: a pipeline's picture ring on its producer device (glue). Each
//! slot is an NV12 texture made with `SHARED_NTHANDLE | SHARED_KEYEDMUTEX`
//! (shader-resource bound: the compositor samples it), with its NT handle
//! (`IDXGIResource1::CreateSharedHandle`, once per texture). A write copies
//! a decoded picture (a slice of the decoder's texture array) into a free
//! slot under the slot's keyed mutex and hands it over as a
//! [`GpuFrame`]. Every decision (which slot, the size classes, the wait,
//! the keys, a lost device) is `crate::ring_state`'s, the shared state
//! `crate::ring`'s; this module only calls Direct3D. Pinned on two WARP
//! devices by `tests/ring.rs`.

use std::sync::Arc;

use tracing::debug;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX,
    D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
use windows::Win32::Graphics::Dxgi::{
    DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIKeyedMutex, IDXGIResource1,
};
use windows::core::{Interface, PCWSTR};

use super::device::{self, DeviceUse};
use super::failed;
use super::probe::acquire;
use super::textures::{self, NtHandle};
use crate::error::GpuError;
use crate::ring::{GpuFrame, RingCore, RingStats, Taken, Written, hand_over};
use crate::ring_state::{
    Acquired, KEY_CONSUMER, RING_WAIT, RingFormat, SizeClass, WRITER_ACQUIRE_MS, copy_box,
    make_texture, missing_subresource, stale_texture, writer_step,
};

/// A slot taken for a write holds no texture after the write made one: a
/// ring bug, reported instead of writing elsewhere.
const NO_SLOT_TEXTURE: GpuError = GpuError::NoObject {
    call: "the ring slot's texture",
};

/// One slot's texture on the producer device, under the ring's allocation
/// number. Its NT handle is closed once the texture's last frame and the
/// ring's own reference are gone (every frame keeps it, so the compositor
/// can open it whatever the ring does meanwhile).
struct SlotTexture {
    alloc_gen: u64,
    texture: ID3D11Texture2D,
    mutex: IDXGIKeyedMutex,
    handle: Arc<NtHandle>,
}

/// A pipeline's ring of shared NV12 textures on its producer device (#279
/// plan §5.3): what its decoded pictures are copied into, each handed over
/// as a [`GpuFrame`] the compositor samples in place. `Send`: its device is
/// thread-safe and its immediate context is used by one thread at a time
/// (the decode thread that owns the ring; a video device is also
/// multithread-protected for Media Foundation's own threads).
pub struct PictureRing {
    core: Arc<RingCore>,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    textures: Vec<Option<SlotTexture>>,
}

impl PictureRing {
    /// A ring of `capacity` slots on `device` (a pipeline's producer
    /// device; `ring_state::RING_CAPACITY` in production). No texture is
    /// made before its first write.
    pub fn new(device: &ID3D11Device, capacity: usize) -> Result<Self, GpuError> {
        let context = unsafe { device.GetImmediateContext() }
            .map_err(|e| failed("GetImmediateContext", &e))?;
        Ok(Self {
            core: RingCore::new(capacity),
            device: device.clone(),
            context,
            textures: (0..capacity).map(|_| None).collect(),
        })
    }

    /// A ring of `capacity` slots on a WARP device of its own, for tests and
    /// CI (WARP refuses the video API, so a test writes its decoder-like
    /// textures on [`device`](Self::device) itself). Not for production.
    #[doc(hidden)]
    pub fn new_warp(capacity: usize) -> Result<Self, GpuError> {
        let ((device, _context), _adapter) = device::create_warp(DeviceUse::Probe)?;
        Self::new(&device, capacity)
    }

    /// The producer device the slots are made on.
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// The ring's counters.
    pub fn stats(&self) -> RingStats {
        self.core.stats()
    }

    /// Copy the top-left `visible` picture of subresource `subresource` of
    /// `source` (an NV12 texture of this device: a slice of a decoder's
    /// texture array, its index the decoder's subresource) into a free slot
    /// and hand it over. The box is the picture rounded up to even sides
    /// (`ring_state::copy_box`), which must lie inside `source`. Under the
    /// slot's keyed mutex: its last key, the copy, a flush (a shared texture
    /// updated on one device must be flushed there), release key 1. While
    /// every slot is in use the write waits for a frame to drop, at most
    /// `RING_WAIT` ([`GpuError::RingFull`]). A slot whose key the writer
    /// does not get within `WRITER_ACQUIRE_MS` (`ring_state::writer_step`:
    /// [`GpuError::RingDead`]), or whose release fails (that call's error),
    /// kills the ring: every later write is `RingDead`, and the reader goes
    /// on in software.
    pub fn copy_from(
        &mut self,
        source: &ID3D11Texture2D,
        subresource: u32,
        visible: (u32, u32),
    ) -> Result<GpuFrame, GpuError> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_NV12 {
            return Err(GpuError::RingSourceFormat {
                format: desc.Format.0 as u32,
            });
        }
        if let Some(count) = missing_subresource(subresource, desc.ArraySize, desc.MipLevels) {
            return Err(GpuError::RingSubresource { subresource, count });
        }
        let copy = copy_box(visible, (desc.Width, desc.Height)).ok_or(GpuError::RingPicture {
            width: visible.0,
            height: visible.1,
            surface_width: desc.Width,
            surface_height: desc.Height,
        })?;
        let class = SizeClass::of(RingFormat::Nv12, visible.0, visible.1);
        let taken = self.core.take(class, RING_WAIT)?;
        match self.write(source, subresource, copy, class, taken) {
            Ok(handle) => Ok(hand_over(
                &self.core,
                Written {
                    slot: taken.slot,
                    alloc_gen: taken.alloc_gen,
                    class,
                    size: visible,
                    handle: handle.0,
                    keep: Some(handle),
                },
            )),
            Err(error) => {
                self.core.release(taken.slot, taken.alloc_gen);
                Err(error)
            }
        }
    }

    /// The copy into the slot `taken` (its texture made first when the take
    /// asks); the slot's NT handle.
    fn write(
        &mut self,
        source: &ID3D11Texture2D,
        subresource: u32,
        (width, height): (u32, u32),
        class: SizeClass,
        taken: Taken,
    ) -> Result<Arc<NtHandle>, GpuError> {
        self.drop_stale_textures();
        let held_gen = self.textures[taken.slot]
            .as_ref()
            .map(|slot| slot.alloc_gen);
        if make_texture(taken.create, held_gen, taken.alloc_gen) {
            self.textures[taken.slot] = Some(self.make(class, taken.alloc_gen)?);
        }
        let slot = self.textures[taken.slot].as_ref().ok_or(NO_SLOT_TEXTURE)?;
        let answer = Acquired::of(acquire(&slot.mutex, taken.key, WRITER_ACQUIRE_MS));
        if let Err(why) = writer_step(answer) {
            self.core.kill(why, Some(answer.hresult()));
            return Err(GpuError::RingDead);
        }
        let region = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: width,
            bottom: height,
            back: 1,
        };
        // SAFETY: both textures live for the call; the box lies inside the
        // source (`copy_box`) and inside the slot (its class has the box's
        // even size).
        unsafe {
            self.context.CopySubresourceRegion(
                &slot.texture,
                0,
                0,
                0,
                0,
                source,
                subresource,
                Some(&region),
            );
            self.context.Flush();
        }
        if let Err(e) = unsafe { slot.mutex.ReleaseSync(KEY_CONSUMER) } {
            self.core.kill(
                "the writer's release of a slot failed",
                Some(e.code().0 as u32),
            );
            return Err(failed(
                "IDXGIKeyedMutex::ReleaseSync (a ring slot, the writer)",
                &e,
            ));
        }
        Ok(Arc::clone(&slot.handle))
    }

    /// A shared NV12 texture of `class` and its keyed mutex and NT handle.
    fn make(&self, class: SizeClass, alloc_gen: u64) -> Result<SlotTexture, GpuError> {
        let misc = (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0)
            as u32;
        let desc = textures::desc(
            class.width,
            class.height,
            DXGI_FORMAT_NV12,
            D3D11_USAGE_DEFAULT,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
            0,
            misc,
        );
        let texture = textures::texture(&self.device, &desc)?;
        let mutex: IDXGIKeyedMutex = texture
            .cast()
            .map_err(|e| failed("IDXGIKeyedMutex (a ring slot)", &e))?;
        let resource: IDXGIResource1 = texture
            .cast()
            .map_err(|e| failed("IDXGIResource1 (a ring slot)", &e))?;
        let access = DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0;
        let handle = unsafe { resource.CreateSharedHandle(None, access, PCWSTR::null()) }
            .map_err(|e| failed("IDXGIResource1::CreateSharedHandle (a ring slot)", &e))?;
        debug!(
            width = class.width,
            height = class.height,
            alloc_gen,
            "sp-gpu: ring slot texture made (NV12, NT handle, keyed mutex)"
        );
        Ok(SlotTexture {
            alloc_gen,
            texture,
            mutex,
            handle: Arc::new(NtHandle(handle.0 as isize)),
        })
    }

    /// Let go of the texture of every slot the ring emptied (a class no
    /// longer kept) or made again: its last frame dropped, so nothing reads
    /// it any more.
    fn drop_stale_textures(&mut self) {
        for (index, texture) in self.textures.iter_mut().enumerate() {
            let state = self.core.slot(index);
            if texture
                .as_ref()
                .is_some_and(|held| stale_texture(state, held.alloc_gen))
            {
                *texture = None;
            }
        }
    }
}
