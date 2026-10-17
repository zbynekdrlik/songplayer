//! #279 P2: the compositor's ring slots (glue). A ring frame's slot is
//! opened ONCE on the compositor's device (`OpenSharedResource1` of its NT
//! handle) with its two plane views (R8 luma, R8G8 chroma: the views the
//! P0 probe proved), kept while it is drawn (`ring_state::OpenedKeys`: at
//! most `OPENED_MAX`, an older texture of the slot closed when its new one
//! is drawn, a slot no compose drew for `OPENED_IDLE_COMPOSES` closed), and
//! held for ONE compose: acquired with key 1 and a 0 ms timeout before the
//! draw (the compositor never waits on a producer), released with key 1
//! after the GPU finished the compose. What a slot's answer means is
//! `ring_state::slot_draw` (drawn, black, its ring killed, or the
//! compositor's own lost device) and how it is counted
//! `ComposeStats::count_slot`; this module only calls Direct3D.

use std::collections::HashMap;

use tracing::warn;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Device1, ID3D11ShaderResourceView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM,
};
use windows::Win32::Graphics::Dxgi::IDXGIKeyedMutex;
use windows::core::Interface;

use super::probe::acquire;
use super::{failed, textures};
use crate::error::GpuError;
use crate::ring::GpuFrame;
use crate::ring_state::{
    Acquired, CONSUMER_ACQUIRE_MS, KEY_CONSUMER, OpenedKeys, SlotDraw, SlotKey, SlotOpen,
    slot_draw, why_killed,
};
use crate::stats::ComposeStats;

/// A slot opened on the compositor's device: the texture, its keyed mutex
/// and its plane views.
struct Opened {
    /// Kept so the views and the mutex refer to a live texture.
    #[allow(dead_code)]
    texture: ID3D11Texture2D,
    mutex: IDXGIKeyedMutex,
    luma: ID3D11ShaderResourceView,
    chroma: ID3D11ShaderResourceView,
}

/// The compositor's open ring slots.
#[derive(Default)]
pub(super) struct RingSlots {
    opened: HashMap<SlotKey, Opened>,
    keys: OpenedKeys,
}

/// One slot held for a compose: its key, its mutex (released on drop) and
/// its plane views.
struct HeldSlot {
    key: SlotKey,
    mutex: IDXGIKeyedMutex,
    luma: ID3D11ShaderResourceView,
    chroma: ID3D11ShaderResourceView,
}

/// The slots one compose holds. Dropping it releases each with key 1:
/// after the compose's wait, or on any error before it.
pub(super) struct Held(Vec<HeldSlot>);

impl Held {
    /// The luma and chroma views of `frame`'s slot, when it is held (else
    /// the side is drawn black).
    pub fn views(
        &self,
        frame: &GpuFrame,
    ) -> Option<(&ID3D11ShaderResourceView, &ID3D11ShaderResourceView)> {
        let key = frame.key();
        self.0
            .iter()
            .find(|held| held.key == key)
            .map(|held| (&held.luma, &held.chroma))
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        for held in &self.0 {
            // SAFETY: the mutex was acquired by this compose with key 1. A
            // failed release (a device reset mid-compose) leaves the key
            // held, abandoned once the compositor is dropped: the next
            // acquire of it, the writer's or the rebuilt compositor's,
            // kills the ring (`ring_state::writer_step`, `slot_draw`).
            let _ = unsafe { held.mutex.ReleaseSync(KEY_CONSUMER) };
        }
    }
}

impl RingSlots {
    /// Open (once) and acquire every ring frame of a compose (`frames`:
    /// `Composition::frames`, each slot once), counting each in `stats`;
    /// first close the slots no compose drew for the idle bound. A slot
    /// that is not drawn is left out (black). The compositor's own lost
    /// device is the error.
    pub fn hold(
        &mut self,
        device: &ID3D11Device,
        frames: &[&GpuFrame],
        stats: &mut ComposeStats,
    ) -> Result<Held, GpuError> {
        for idle in self.keys.begin_compose() {
            self.opened.remove(&idle);
        }
        let mut held = Held(Vec::with_capacity(frames.len()));
        for frame in frames {
            let key = frame.key();
            let touch = self.keys.touch(key);
            for closed in &touch.close {
                self.opened.remove(closed);
            }
            let open = if frame.ring_dead() {
                SlotOpen::Dead
            } else {
                self.open_and_acquire(device, frame, touch.open, stats)
            };
            let mut removed = None;
            let draw = slot_draw(open, || {
                removed = removed_reason(device);
                removed.is_some()
            });
            stats.count_slot(open, draw);
            match draw {
                SlotDraw::Draw => {
                    if let Some(opened) = self.opened.get(&key) {
                        held.0.push(HeldSlot {
                            key,
                            mutex: opened.mutex.clone(),
                            luma: opened.luma.clone(),
                            chroma: opened.chroma.clone(),
                        });
                    }
                }
                SlotDraw::Black => {}
                SlotDraw::Gone => {
                    self.keys.forget(key);
                    self.opened.remove(&key);
                }
                SlotDraw::KillRing => {
                    self.keys.forget(key);
                    self.opened.remove(&key);
                    let hresult = match open {
                        SlotOpen::Acquired(answer) => Some(answer.hresult()),
                        _ => None,
                    };
                    frame.kill_ring(why_killed(open), hresult);
                }
                SlotDraw::DeviceLost => {
                    return Err(GpuError::removed(removed.unwrap_or_default()));
                }
            }
        }
        Ok(held)
    }

    /// `frame`'s slot opened (when `open_now`, or not open yet) and its
    /// keyed mutex's answer to key 1 with a 0 ms timeout; `Failed` when it
    /// does not open.
    fn open_and_acquire(
        &mut self,
        device: &ID3D11Device,
        frame: &GpuFrame,
        open_now: bool,
        stats: &mut ComposeStats,
    ) -> SlotOpen {
        let key = frame.key();
        if open_now || !self.opened.contains_key(&key) {
            match open(device, frame) {
                Ok(opened) => {
                    self.opened.insert(key, opened);
                    stats.gpu_opens += 1;
                }
                Err(error) => {
                    warn!(%error, ?key, "sp-gpu: a ring slot did not open on the compositor");
                    self.keys.forget(key);
                    return SlotOpen::Failed;
                }
            }
        }
        match self.opened.get(&key) {
            Some(opened) => SlotOpen::Acquired(Acquired::of(acquire(
                &opened.mutex,
                KEY_CONSUMER,
                CONSUMER_ACQUIRE_MS,
            ))),
            None => SlotOpen::Failed,
        }
    }
}

/// The compositor device's removed reason, when it was removed.
fn removed_reason(device: &ID3D11Device) -> Option<u32> {
    unsafe { device.GetDeviceRemovedReason() }
        .err()
        .map(|e| e.code().0 as u32)
}

/// `frame`'s slot texture opened on `device`, with its mutex and views.
fn open(device: &ID3D11Device, frame: &GpuFrame) -> Result<Opened, GpuError> {
    let device1: ID3D11Device1 = device
        .cast()
        .map_err(|e| failed("ID3D11Device1 (the compositor)", &e))?;
    let handle = HANDLE(frame.shared_handle() as *mut std::ffi::c_void);
    // SAFETY: the handle is the slot's NT handle, open while `frame` lives
    // (every frame keeps it).
    let texture: ID3D11Texture2D = unsafe { device1.OpenSharedResource1(handle) }
        .map_err(|e| failed("ID3D11Device1::OpenSharedResource1 (a ring slot)", &e))?;
    let mutex: IDXGIKeyedMutex = texture
        .cast()
        .map_err(|e| failed("IDXGIKeyedMutex (an opened ring slot)", &e))?;
    let luma = plane_view(device, &texture, DXGI_FORMAT_R8_UNORM)?;
    let chroma = plane_view(device, &texture, DXGI_FORMAT_R8G8_UNORM)?;
    Ok(Opened {
        texture,
        mutex,
        luma,
        chroma,
    })
}

/// A view of one plane of the opened NV12 texture
/// (`textures::plane_view_desc`).
fn plane_view(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
    format: DXGI_FORMAT,
) -> Result<ID3D11ShaderResourceView, GpuError> {
    const CALL: &str = "CreateShaderResourceView (a ring slot's plane)";
    let desc = textures::plane_view_desc(format);
    let mut view: Option<ID3D11ShaderResourceView> = None;
    unsafe { device.CreateShaderResourceView(texture, Some(&desc), Some(&mut view)) }
        .map_err(|e| failed(CALL, &e))?;
    view.ok_or(GpuError::NoObject { call: CALL })
}
