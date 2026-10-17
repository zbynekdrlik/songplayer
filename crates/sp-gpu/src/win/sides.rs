//! #279 P2: the compositor's ring slots (glue). A ring frame's slot is
//! opened ONCE on the compositor's device (`OpenSharedResource1` of its NT
//! handle) with its two plane views (R8 luma, R8G8 chroma: the views the
//! P0 probe proved), kept while it is drawn (`ring_state::OpenedKeys`:
//! at most `OPENED_MAX`, an older texture of the slot closed when its new
//! one is drawn), and held for ONE compose: acquired with key 1 and a 0 ms
//! timeout before the draw (the compositor never waits on a producer),
//! released with key 1 after the GPU finished the compose. A slot that is
//! busy, abandoned (its producer device lost), fails its acquire or does
//! not open is drawn black and counted (`ComposeStats::count_gpu_side`);
//! a lost one kills its ring. Every decision is `crate::ring_state`'s and
//! `crate::stats`'s.

use std::collections::HashMap;
use std::ffi::c_void;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D::D3D11_SRV_DIMENSION_TEXTURE2D;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_TEX2D_SRV,
    ID3D11Device, ID3D11Device1, ID3D11ShaderResourceView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM,
};
use windows::Win32::Graphics::Dxgi::IDXGIKeyedMutex;
use windows::core::Interface;

use super::failed;
use super::probe::acquire;
use crate::error::GpuError;
use crate::ring::GpuFrame;
use crate::ring_state::{Acquired, CONSUMER_ACQUIRE_MS, KEY_CONSUMER, OpenedKeys, SlotKey};
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
            // SAFETY: the mutex was acquired by this compose with key 1;
            // a failed release leaves it to the producer's bounded acquire.
            let _ = unsafe { held.mutex.ReleaseSync(KEY_CONSUMER) };
        }
    }
}

impl RingSlots {
    /// Open (once) and acquire every ring frame of a compose (`frames`:
    /// `Composition::frames`, each slot once), counting each in `stats`.
    /// A slot that does not open, is busy, abandoned or fails its acquire
    /// is left out (drawn black); a lost or failed one kills its ring. A
    /// lost compositor device is the error.
    pub fn hold(
        &mut self,
        device: &ID3D11Device,
        frames: &[&GpuFrame],
        stats: &mut ComposeStats,
    ) -> Result<Held, GpuError> {
        let mut held = Held(Vec::with_capacity(frames.len()));
        for frame in frames {
            let key = frame.key();
            let touch = self.keys.touch(key);
            for closed in &touch.close {
                self.opened.remove(closed);
            }
            if touch.open {
                match open(device, frame) {
                    Ok(opened) => {
                        self.opened.insert(key, opened);
                        stats.gpu_opens += 1;
                    }
                    Err(error) if error.is_device_lost() => {
                        self.keys.forget(key);
                        return Err(error);
                    }
                    Err(_) => {
                        self.keys.forget(key);
                        stats.abandoned += 1;
                        frame.kill_ring();
                        continue;
                    }
                }
            }
            let Some(opened) = self.opened.get(&key) else {
                continue;
            };
            let answer = Acquired::of(acquire(&opened.mutex, KEY_CONSUMER, CONSUMER_ACQUIRE_MS));
            if stats.count_gpu_side(answer) {
                held.0.push(HeldSlot {
                    key,
                    mutex: opened.mutex.clone(),
                    luma: opened.luma.clone(),
                    chroma: opened.chroma.clone(),
                });
            } else if answer.kills_ring() {
                frame.kill_ring();
            }
        }
        Ok(held)
    }
}

/// `frame`'s slot texture opened on `device`, with its mutex and views.
fn open(device: &ID3D11Device, frame: &GpuFrame) -> Result<Opened, GpuError> {
    let device1: ID3D11Device1 = device
        .cast()
        .map_err(|e| failed("ID3D11Device1 (the compositor)", &e))?;
    let handle = HANDLE(frame.shared_handle() as *mut c_void);
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

/// A view of one plane of the opened NV12 texture (R8 = luma, R8G8 = the
/// interleaved chroma at half size: Microsoft's NV12 view rule).
fn plane_view(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
    format: DXGI_FORMAT,
) -> Result<ID3D11ShaderResourceView, GpuError> {
    const CALL: &str = "CreateShaderResourceView (a ring slot's plane)";
    let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: format,
        ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
            },
        },
    };
    let mut view: Option<ID3D11ShaderResourceView> = None;
    unsafe { device.CreateShaderResourceView(texture, Some(&desc), Some(&mut view)) }
        .map_err(|e| failed(CALL, &e))?;
    view.ok_or(GpuError::NoObject { call: CALL })
}
