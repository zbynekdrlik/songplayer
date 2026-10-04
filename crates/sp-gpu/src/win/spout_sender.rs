//! The Spout sender of `SP-program-MAX` (#223 S1b): the compositor's render
//! target, shared with Resolume Arena through the vendored Spout2 SDK
//! (SpoutDX `SendTexture`, via `spout_shim.cpp`). Every decision (the name
//! check, refusing a taken name, confirming or refusing the first send) is
//! `crate::spout` / `crate::spout_state`'s, tested on Linux; this file only
//! calls the shim and carries the state.

use std::ffi::{c_char, c_int, c_void};
use std::fmt;
use std::ptr::NonNull;
use std::time::Instant;

use tracing::{info, warn};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::core::Interface;

use super::{Compositor, micros_since};
use crate::error::GpuError;
use crate::spout::{SPOUT_SENDER_NAME, check_sender_name, status_result};
use crate::spout_state::{
    AfterSend, BeforeSend, FirstSendFacts, Listed, Registration, after_send, before_send, claim,
};
use crate::stats::SpoutSendStats;

// `spout_shim.cpp`, built with the vendored SDK by build.rs.
unsafe extern "C" {
    fn spout_sender_open(
        device: *mut c_void,
        name: *const c_char,
        status: *mut c_int,
    ) -> *mut c_void;
    fn spout_sender_listed(sender: *mut c_void) -> c_int;
    fn spout_sender_claim_name(sender: *mut c_void) -> c_int;
    fn spout_sender_send(sender: *mut c_void, texture: *mut c_void) -> c_int;
    fn spout_sender_state(sender: *mut c_void, initialized: *mut c_int, name_matches: *mut c_int);
    fn spout_sender_refuse(sender: *mut c_void) -> c_int;
    fn spout_sender_size(sender: *mut c_void, width: *mut u32, height: *mut u32);
    fn spout_sender_release(sender: *mut c_void);
}

/// The shim's sender object, released when dropped: `~spoutDX` unregisters
/// it. A `SpoutSender` declares it before the device, so it is released
/// while the device (which spoutDX does not AddRef) still lives.
struct Shim(NonNull<c_void>);

impl Shim {
    fn raw(&self) -> *mut c_void {
        self.0.as_ptr()
    }
}

impl Drop for Shim {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `spout_sender_open` and is released
        // once, here.
        unsafe { spout_sender_release(self.0.as_ptr()) };
    }
}

/// What Spout's list says about the sender's name now.
fn listed_now(raw: *mut c_void) -> Listed {
    // SAFETY: `raw` is a live shim sender (its `Shim` outlives the call).
    Listed::from_code(unsafe { spout_sender_listed(raw) })
}

/// What spoutDX holds after a send of a sender not yet confirmed.
fn first_send_facts(raw: *mut c_void) -> FirstSendFacts {
    let (mut initialized, mut name_matches): (c_int, c_int) = (0, 0);
    // SAFETY: `raw` is a live shim sender; both out slots are valid.
    unsafe { spout_sender_state(raw, &mut initialized, &mut name_matches) };
    FirstSendFacts {
        initialized: initialized != 0,
        name_matches: name_matches != 0,
        listed: listed_now(raw),
    }
}

/// A Spout sender of the compositor's render target. It holds its own
/// references to the compositor's device and render target (Spout does not
/// AddRef the device), so it may outlive the `Compositor` value; S2 drops it
/// with the compositor when the device is lost. Registered at its first
/// [`send`](SpoutSender::send), unregistered when dropped.
///
/// Not `Send`: it drives the compositor's immediate context, which is not
/// thread-safe, so it lives on the thread that composes (S2 builds both on
/// `program-max`).
pub struct SpoutSender {
    // Dropped first (declaration order): released while `device` lives.
    shim: Shim,
    name: String,
    registration: Registration,
    device: ID3D11Device,
    texture: ID3D11Texture2D,
}

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
        // SAFETY: `device` is a live ID3D11Device, kept alive until after the
        // shim object is released (see `Shim`); `c_name` is NUL-terminated
        // and outlives the call; `code` is a valid out slot.
        let raw = unsafe { spout_sender_open(device.as_raw(), c_name.as_ptr(), &mut code) };
        status_result(code, "spout_sender_open", name)?;
        let shim = Shim(NonNull::new(raw).ok_or(GpuError::NoObject {
            call: "spout_sender_open",
        })?);
        let listed = listed_now(shim.raw());
        // SAFETY: `shim` is a live shim sender.
        claim(
            listed,
            || unsafe { spout_sender_claim_name(shim.raw()) },
            name,
        )?;
        info!(
            name,
            adapter = %compositor.adapter().name,
            "sp-gpu: Spout sender ready (Spout lists it from its first send)"
        );
        Ok(Self {
            shim,
            name: name.to_owned(),
            registration: Registration::Fresh,
            device,
            texture,
        })
    }

    /// The name the sender registers.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Where the sender is in its registration with Spout.
    pub fn registration(&self) -> Registration {
        self.registration
    }

    /// Share the render target as it is now: call it after
    /// `Compositor::compose`, on the same thread. Spout copies it into its
    /// own shared texture on the GPU (`SendTexture`); the first call
    /// registers the sender. [`GpuError::DeviceLost`] means the device must
    /// be rebuilt (and the sender with it). [`GpuError::SpoutNotRegistered`]
    /// means this sender is done (`crate::spout_state`: another sender took
    /// the name, Spout did not list it, or could not register it): drop it
    /// and make a new one. A [`GpuError::Spout`] is one lost frame.
    ///
    /// Spout skips the copy, and still reports success, when a receiver
    /// holds the sender's mutex for over 67 ms: `send_us` then shows the
    /// wait.
    pub fn send(&mut self) -> Result<SpoutSendStats, GpuError> {
        let raw = self.shim.raw();
        match before_send(self.registration, || listed_now(raw)) {
            BeforeSend::Send => {}
            BeforeSend::Refused(why) => return Err(self.not_registered(why)),
            BeforeSend::Refuse(why) => return Err(self.refuse(why)),
        }
        let start = Instant::now();
        // SAFETY: the shim sender is live until Drop; the texture is the
        // compositor's render target, on the device the sender opened on.
        let code = unsafe { spout_sender_send(raw, self.texture.as_raw()) };
        let send_us = micros_since(start);
        unsafe { self.device.GetDeviceRemovedReason() }
            .map_err(|e| GpuError::removed(e.code().0 as u32))?;
        match after_send(self.registration, code, || first_send_facts(raw)) {
            AfterSend::Shared(next) => {
                if next == Registration::Confirmed && self.registration != next {
                    info!(name = %self.name, "sp-gpu: Spout lists the sender");
                }
                self.registration = next;
                Ok(SpoutSendStats { send_us })
            }
            AfterSend::Lost(code) => Err(GpuError::Spout {
                call: "spout_sender_send",
                code: code as u32,
            }),
            AfterSend::Refuse(why) => Err(self.refuse(why)),
        }
    }

    /// The size Spout shares, as SpoutDX reports it: (0, 0) before the
    /// first send and once refused, then the render target's 3840×2160.
    pub fn size(&self) -> (u32, u32) {
        let (mut width, mut height) = (0, 0);
        // SAFETY: the shim sender is live until Drop; both out slots are
        // valid.
        unsafe { spout_sender_size(self.shim.raw(), &mut width, &mut height) };
        (width, height)
    }

    /// Refuse the sender for good: release what it registered.
    fn refuse(&mut self, why: &'static str) -> GpuError {
        // SAFETY: the shim sender is live until Drop.
        let code = unsafe { spout_sender_refuse(self.shim.raw()) };
        if let Err(error) = status_result(code, "spout_sender_refuse", &self.name) {
            warn!(name = %self.name, %error, "sp-gpu: releasing a refused Spout sender failed");
        }
        warn!(name = %self.name, why, "sp-gpu: Spout sender refused for good");
        self.registration = Registration::Refused(why);
        self.not_registered(why)
    }

    fn not_registered(&self, why: &'static str) -> GpuError {
        GpuError::SpoutNotRegistered {
            name: self.name.clone(),
            why,
        }
    }
}

impl fmt::Debug for SpoutSender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpoutSender")
            .field("name", &self.name)
            .field("registration", &self.registration)
            .finish_non_exhaustive()
    }
}

impl Drop for SpoutSender {
    fn drop(&mut self) {
        // The fields drop after this: `shim` first (unregisters), then the
        // device and the render target.
        info!(name = %self.name, "sp-gpu: Spout sender released");
    }
}
