//! The Spout sender of `SP-program-MAX` (#223 S1b): the compositor's render
//! target, shared with Resolume Arena through the vendored Spout2 SDK
//! (SpoutDX `SendTexture`, via `spout_shim.cpp`). The decisions (the name
//! check, the status codes) are `crate::spout`'s, tested on Linux; this file
//! only calls the shim.

use std::ffi::{c_char, c_int, c_void};
use std::fmt;
use std::ptr::NonNull;
use std::time::Instant;

use tracing::info;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::core::Interface;

use super::{Compositor, micros_since};
use crate::error::GpuError;
use crate::spout::{SPOUT_SENDER_NAME, check_sender_name, status_result};
use crate::stats::SpoutSendStats;

// `spout_shim.cpp`, built with the vendored SDK by build.rs.
unsafe extern "C" {
    fn spout_sender_create(
        device: *mut c_void,
        name: *const c_char,
        status: *mut c_int,
    ) -> *mut c_void;
    fn spout_sender_send(sender: *mut c_void, texture: *mut c_void) -> c_int;
    fn spout_sender_size(sender: *mut c_void, width: *mut u32, height: *mut u32);
    fn spout_sender_release(sender: *mut c_void);
}

/// A Spout sender of the compositor's render target. It holds its own
/// references to the compositor's device and render target (Spout does not
/// AddRef the device), so it may outlive the `Compositor` value; S2 drops it
/// with the compositor when the device is lost. Registered at its first
/// [`send`](SpoutSender::send), unregistered when dropped. `Send` but not
/// `Sync`: it uses the device's immediate context, so it sends on the
/// thread that composes (S2's `program-max`).
pub struct SpoutSender {
    handle: NonNull<c_void>,
    name: String,
    device: ID3D11Device,
    texture: ID3D11Texture2D,
}

// SAFETY: the shim's `spoutDX` has no thread affinity between calls: each
// call takes and releases its named mutex itself (SendTexture's
// CheckTextureAccess/AllowTextureAccess, SpoutSharedMemory Lock/Unlock), the
// frame-count semaphore has no owner, and it keeps no thread-local state. The
// D3D11 device, texture and context it uses are `Send` (windows 0.58). So
// moving the sender to another thread is sound; `&mut self` on every call
// keeps two threads from using it at once (it is not `Sync`).
unsafe impl Send for SpoutSender {}

impl SpoutSender {
    /// The sender `SP-program-MAX` ([`SPOUT_SENDER_NAME`]) on `compositor`'s
    /// device. [`GpuError::SpoutNameTaken`] when a live sender holds the
    /// name (Spout would have renamed this one).
    pub fn new(compositor: &Compositor) -> Result<Self, GpuError> {
        Self::with_name(compositor, SPOUT_SENDER_NAME)
    }

    /// The sender under another name: tests run several at once, and the
    /// production name is one per machine. Not for production.
    #[doc(hidden)]
    pub fn with_name(compositor: &Compositor, name: &str) -> Result<Self, GpuError> {
        let c_name = check_sender_name(name)?;
        let device = compositor.device().clone();
        let texture = compositor.render_target().clone();
        // Not OK until the shim says so.
        let mut code: c_int = -1;
        // SAFETY: `device` is a live ID3D11Device, kept alive by the sender
        // it is stored in until after `spout_sender_release`; `c_name` is
        // NUL-terminated and outlives the call; `code` is a valid out slot.
        let raw = unsafe { spout_sender_create(device.as_raw(), c_name.as_ptr(), &mut code) };
        status_result(code, "spout_sender_create", name)?;
        let handle = NonNull::new(raw).ok_or(GpuError::NoObject {
            call: "spout_sender_create",
        })?;
        info!(
            name,
            adapter = %compositor.adapter().name,
            "sp-gpu: Spout sender ready (Spout lists it from its first send)"
        );
        Ok(Self {
            handle,
            name: name.to_owned(),
            device,
            texture,
        })
    }

    /// The name the sender registers.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Share the render target as it is now: call it after
    /// `Compositor::compose`, on the same thread. Spout copies it into its
    /// own shared texture on the GPU (`SendTexture`); the first call
    /// registers the sender. [`GpuError::DeviceLost`] means the device must
    /// be rebuilt (and the sender with it); [`GpuError::SpoutNotRegistered`]
    /// (the first send: Spout registered another name, did not list it, or
    /// the send failed) means this sender is done: drop it and make a new
    /// one. A later [`GpuError::Spout`] is one failed frame.
    ///
    /// Spout skips the copy, and still reports success, when a receiver
    /// holds the sender's mutex for over 67 ms: `send_us` then shows the
    /// wait.
    pub fn send(&mut self) -> Result<SpoutSendStats, GpuError> {
        let start = Instant::now();
        // SAFETY: the handle is live until Drop; the texture is the
        // compositor's render target, on the device the sender opened on.
        let code = unsafe { spout_sender_send(self.handle.as_ptr(), self.texture.as_raw()) };
        let send_us = micros_since(start);
        unsafe { self.device.GetDeviceRemovedReason() }
            .map_err(|e| GpuError::removed(e.code().0 as u32))?;
        status_result(code, "spout_sender_send", &self.name)?;
        Ok(SpoutSendStats { send_us })
    }

    /// The size Spout shares, as SpoutDX reports it: (0, 0) before the
    /// first send, then the render target's 3840×2160.
    pub fn size(&self) -> (u32, u32) {
        let (mut width, mut height) = (0, 0);
        // SAFETY: the handle is live until Drop; both out slots are valid.
        unsafe { spout_sender_size(self.handle.as_ptr(), &mut width, &mut height) };
        (width, height)
    }
}

impl fmt::Debug for SpoutSender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpoutSender")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl Drop for SpoutSender {
    fn drop(&mut self) {
        // SAFETY: the handle came from `spout_sender_create` and is released
        // once, here, while `self.device` (dropped after this body) lives.
        unsafe { spout_sender_release(self.handle.as_ptr()) };
        info!(name = %self.name, "sp-gpu: Spout sender released");
    }
}
