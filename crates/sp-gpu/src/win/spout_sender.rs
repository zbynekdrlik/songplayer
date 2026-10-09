//! The Spout sender of `SP-program-MAX` (#223 S1b), and of the FHD program
//! `SP-program` (#239): the compositor's render target, shared with Resolume
//! Arena through the vendored Spout2 SDK
//! (SpoutDX `SendTexture`, via `spout_shim.cpp`). Every decision (the name
//! check, refusing a taken name, confirming or refusing the first send) is
//! `crate::spout` / `crate::spout_state`'s, tested on Linux; this file only
//! calls the shim and carries the state.

use std::ffi::{c_char, c_int, c_void};
use std::fmt;
use std::ptr::NonNull;
use std::sync::Mutex;
use std::time::Instant;

use tracing::{info, warn};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Query, ID3D11Texture2D,
};
use windows::core::Interface;

use super::{Compositor, micros_since};
use crate::error::GpuError;
use crate::spout::{SPOUT_FHD_SENDER_NAME, SPOUT_SENDER_NAME, check_sender_name, status_result};
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

/// One Spout SDK call at a time in this process. The SDK keeps
/// process-global state (its log buffer, written by every log of level
/// Notice or higher even with logging off), so senders on two threads must
/// never run it at once. Each shim call takes it alone (never nested).
static SDK: Mutex<()> = Mutex::new(());

/// Run one shim call under [`SDK`].
fn sdk<T>(call: impl FnOnce() -> T) -> T {
    let _one = SDK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    call()
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
        let raw = self.0.as_ptr();
        // SAFETY: the pointer came from `spout_sender_open` and is released
        // once, here.
        sdk(|| unsafe { spout_sender_release(raw) });
    }
}

/// What Spout's list says about the sender's name now.
fn listed_now(raw: *mut c_void) -> Listed {
    // SAFETY: `raw` is a live shim sender (its `Shim` outlives the call).
    Listed::from_code(sdk(|| unsafe { spout_sender_listed(raw) }))
}

/// What spoutDX holds after a send of a sender not yet confirmed.
fn first_send_facts(raw: *mut c_void) -> FirstSendFacts {
    let (mut initialized, mut name_matches): (c_int, c_int) = (0, 0);
    // SAFETY: `raw` is a live shim sender; both out slots are valid.
    sdk(|| unsafe { spout_sender_state(raw, &mut initialized, &mut name_matches) });
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
/// Neither it nor the `Compositor` is `Send`: both drive the device's one
/// immediate context, which is not thread-safe, so both live on the thread
/// that made them (S2 builds both on `program-max`).
pub struct SpoutSender {
    // Dropped first (declaration order): released while `device` lives.
    shim: Shim,
    name: String,
    registration: Registration,
    device: ID3D11Device,
    texture: ID3D11Texture2D,
    /// The compositor's immediate context and an event query: each send
    /// waits until the GPU has done its copy into Spout's shared texture.
    context: ID3D11DeviceContext,
    copied: ID3D11Query,
}

impl SpoutSender {
    /// The sender `SP-program-MAX` ([`SPOUT_SENDER_NAME`]) on `compositor`'s
    /// device. [`GpuError::SpoutNameTaken`] when a live sender holds the
    /// name (Spout would have renamed this one).
    pub fn new(compositor: &Compositor) -> Result<Self, GpuError> {
        Self::with_name(compositor, SPOUT_SENDER_NAME)
    }

    /// #239: the sender `SP-program` ([`SPOUT_FHD_SENDER_NAME`]), on the
    /// FHD program's 1920×1080 compositor (`Compositor::with_size`): it
    /// shares whatever `compositor` draws, so pair it with that one.
    /// [`GpuError::SpoutNameTaken`] when a live sender holds the name.
    pub fn new_fhd(compositor: &Compositor) -> Result<Self, GpuError> {
        Self::with_name(compositor, SPOUT_FHD_SENDER_NAME)
    }

    /// The sender under another name: tests run several at once, and the
    /// production name is one per machine. Not for production.
    #[doc(hidden)]
    pub fn with_name(compositor: &Compositor, name: &str) -> Result<Self, GpuError> {
        let c_name = check_sender_name(name)?;
        let device = compositor.device().clone();
        let texture = compositor.render_target().clone();
        let context = compositor.context().clone();
        let copied = super::pipeline::query(&device)?;
        // Not OK until the shim says so.
        let mut code: c_int = -1;
        // SAFETY: `device` is a live ID3D11Device, kept alive until after the
        // shim object is released (see `Shim`); `c_name` is NUL-terminated
        // and outlives the call; `code` is a valid out slot.
        let raw = sdk(|| unsafe { spout_sender_open(device.as_raw(), c_name.as_ptr(), &mut code) });
        status_result(code, "spout_sender_open", name)?;
        let shim = Shim(NonNull::new(raw).ok_or(GpuError::NoObject {
            call: "spout_sender_open",
        })?);
        let raw = shim.raw();
        let listed = listed_now(raw);
        // SAFETY: `shim` is a live shim sender.
        claim(
            listed,
            || sdk(|| unsafe { spout_sender_claim_name(raw) }),
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
            context,
            copied,
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
    /// the name, Spout did not list it or could not register it, or its list
    /// stayed unreadable): drop it and make a new one after a backoff. A
    /// [`GpuError::Spout`] is one lost frame.
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
        let texture = self.texture.as_raw();
        let code = sdk(|| unsafe { spout_sender_send(raw, texture) });
        // #223 follow-up: return once the GPU has done the copy, so the
        // caller's timing is when Spout's shared texture really holds the
        // frame (Arena reads it on its own clock), not when it was queued.
        super::pipeline::wait_until_done_on(&self.context, &self.copied)?;
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
            AfterSend::Lost { code, next } => {
                self.registration = next;
                Err(GpuError::Spout {
                    call: "spout_sender_send",
                    code: code as u32,
                })
            }
            AfterSend::Refuse(why) => Err(self.refuse(why)),
        }
    }

    /// The size Spout shares, as SpoutDX reports it: (0, 0) before the
    /// first send and once refused, then the render target's size.
    pub fn size(&self) -> (u32, u32) {
        let (mut width, mut height) = (0, 0);
        // SAFETY: the shim sender is live until Drop; both out slots are
        // valid.
        let raw = self.shim.raw();
        sdk(|| unsafe { spout_sender_size(raw, &mut width, &mut height) });
        (width, height)
    }

    /// Refuse the sender for good: release what it registered.
    fn refuse(&mut self, why: &'static str) -> GpuError {
        // SAFETY: the shim sender is live until Drop.
        let raw = self.shim.raw();
        let code = sdk(|| unsafe { spout_sender_refuse(raw) });
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
