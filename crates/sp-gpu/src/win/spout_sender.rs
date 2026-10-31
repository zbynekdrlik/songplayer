//! The Spout sender of `SP-program-MAX` (#223 S1b), and of the FHD program
//! `SP-program` (#239): one of the compositor's render targets (#251 step 2:
//! the primary, or the FHD target added to the same compositor), shared
//! with Resolume Arena through the vendored Spout2 SDK
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
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Query, ID3D11Texture2D};
use windows::core::Interface;

use super::spout_lock::{give_back, give_back_maps, held_elsewhere, listed_senders};
use super::{Compositor, NO_TARGET, micros_since};
use crate::error::GpuError;
use crate::spout::{
    SENDER_NAMES_MAP, SPOUT_FHD_SENDER_NAME, SPOUT_SENDER_NAME, check_sender_name, lost_send_error,
    map_mutex_name, shim_send_mode, status_result,
};
use crate::spout_lock::access_mutex_name;
use crate::spout_state::{
    AccessHeld, AfterSend, BeforeSend, FirstSendFacts, ListCheck, Listed, Registration, UNREADABLE,
    access_held, after_send, before_send, claim, list_check,
};
use crate::stats::SpoutSendStats;
use crate::targets::TargetId;
use sp_core::config::SpoutSend;

// `spout_shim.cpp`, built with the vendored SDK by build.rs.
unsafe extern "C" {
    fn spout_sender_open(
        device: *mut c_void,
        name: *const c_char,
        status: *mut c_int,
    ) -> *mut c_void;
    fn spout_sender_listed(sender: *mut c_void) -> c_int;
    fn spout_sender_claim_name(sender: *mut c_void) -> c_int;
    fn spout_sender_send_synced(
        sender: *mut c_void,
        texture: *mut c_void,
        done: *mut c_void,
        bound_ms: u64,
        mode: c_int,
    ) -> c_int;
    fn spout_sender_relist(sender: *mut c_void) -> c_int;
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

/// Run one shim call of the sender `sender` under [`SDK`], then give back
/// any Spout lock the call left this thread holding (#243: Spout keeps a
/// lock it was handed abandoned, `crate::spout_lock`). #251: a send holds
/// [`SDK`] through its wait for the GPU's copy (well under a millisecond,
/// bounded by `pipeline::FRAME_WAIT_LIMIT`); only the `program-max` thread
/// sends, so no other sender waits behind it.
fn sdk<T>(sender: &str, call: impl FnOnce() -> T) -> T {
    let _one = SDK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let out = call();
    give_back(sender);
    out
}

/// [`sdk`] for a call that runs Spout's `CleanSenders` (the open, the name
/// claim). It reads every listed sender's info map under that map's lock,
/// and a lock it meets abandoned makes it drop that sender off the list
/// while this thread keeps the lock (#243 review): so the info-map locks of
/// every sender listed before OR after the call are given back too.
fn sdk_cleaning<T>(sender: &str, call: impl FnOnce() -> T) -> T {
    let _one = SDK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let before = listed_senders();
    let out = call();
    give_back(sender);
    let after = listed_senders();
    give_back_maps(before.iter().chain(&after));
    out
}

/// The shim's sender object, released when dropped: `~spoutDX` unregisters
/// it. A `SpoutSender` declares it before the device, so it is released
/// while the device (which spoutDX does not AddRef) still lives. It keeps
/// the sender's name for the locks its calls give back.
struct Shim {
    raw: NonNull<c_void>,
    name: String,
}

impl Shim {
    fn raw(&self) -> *mut c_void {
        self.raw.as_ptr()
    }
}

impl Drop for Shim {
    fn drop(&mut self) {
        let raw = self.raw.as_ptr();
        // SAFETY: the pointer came from `spout_sender_open` and is released
        // once, here.
        sdk(&self.name, || unsafe { spout_sender_release(raw) });
    }
}

/// What Spout's list says about the sender's name now.
fn listed_now(shim: &Shim) -> Listed {
    let raw = shim.raw();
    // SAFETY: `raw` is a live shim sender (its `Shim` outlives the call).
    Listed::from_code(sdk(&shim.name, || unsafe { spout_sender_listed(raw) }))
}

/// What spoutDX holds after a send of a sender not yet confirmed.
fn first_send_facts(shim: &Shim) -> FirstSendFacts {
    let (mut initialized, mut name_matches): (c_int, c_int) = (0, 0);
    let raw = shim.raw();
    // SAFETY: `raw` is a live shim sender; both out slots are valid.
    sdk(&shim.name, || unsafe {
        spout_sender_state(raw, &mut initialized, &mut name_matches)
    });
    FirstSendFacts {
        initialized: initialized != 0,
        name_matches: name_matches != 0,
        listed: listed_now(shim),
    }
}

/// A Spout sender of one of the compositor's render targets. It holds its
/// own references to the compositor's device and that render target (Spout
/// does not AddRef the device), so it may outlive the `Compositor` value
/// (and its target's removal); S2 drops it with the compositor when the
/// device is lost. Registered at its first [`send`](SpoutSender::send),
/// unregistered when dropped.
///
/// Neither it nor the `Compositor` is `Send`: both drive the device's one
/// immediate context, which is not thread-safe, so both live on the thread
/// that made them (S2 builds both on `program-max`).
pub struct SpoutSender {
    // Dropped first (declaration order): released while `device` lives.
    shim: Shim,
    name: String,
    registration: Registration,
    /// #243: sends in a row after which another program held the sender's
    /// texture lock (`spout_state::access_held`).
    access_held: u32,
    /// #243: sends since the sender was confirmed (`spout_state::list_check`
    /// looks at Spout's list every `RELIST_EVERY`th).
    confirmed_sends: u64,
    device: ID3D11Device,
    texture: ID3D11Texture2D,
    /// An event query on the compositor's device: each send waits until the
    /// GPU has done its copy into Spout's shared texture BEFORE Spout
    /// announces the frame (#251, the shim's `SendSynced`).
    copied: ID3D11Query,
    /// #251 (19.10.2026): how each frame is handed to Spout around its
    /// texture mutex (`program_max_spout_send`, set before every send).
    mode: SpoutSend,
}

impl SpoutSender {
    /// The sender `SP-program-MAX` ([`SPOUT_SENDER_NAME`]) of `compositor`'s
    /// primary target. [`GpuError::SpoutNameTaken`] when a live sender holds
    /// the name (Spout would have renamed this one).
    pub fn new(compositor: &Compositor) -> Result<Self, GpuError> {
        Self::for_target(compositor, TargetId::PRIMARY, SPOUT_SENDER_NAME)
    }

    /// #239: the sender `SP-program` ([`SPOUT_FHD_SENDER_NAME`]) of the FHD
    /// program's 1920×1080 target `target` (#251 step 2: added to MAX's
    /// compositor, `Compositor::add_target` with `FHD_TARGET`).
    /// [`GpuError::SpoutNameTaken`] when a live sender holds the name.
    pub fn new_fhd(compositor: &Compositor, target: TargetId) -> Result<Self, GpuError> {
        Self::for_target(compositor, target, SPOUT_FHD_SENDER_NAME)
    }

    /// The sender of the primary target under another name: tests run
    /// several at once, and the production name is one per machine. Not for
    /// production.
    #[doc(hidden)]
    pub fn with_name(compositor: &Compositor, name: &str) -> Result<Self, GpuError> {
        Self::for_target(compositor, TargetId::PRIMARY, name)
    }

    /// #251 step 2: the sender `name` of `compositor`'s render target
    /// `target`: it shares what each compose draws there. It holds its own
    /// reference to that texture (a removed target goes on existing until
    /// the sender is dropped). [`GpuError::NoObject`] for a target the
    /// compositor does not hold; [`GpuError::SpoutNameTaken`] when a live
    /// sender holds the name.
    pub fn for_target(
        compositor: &Compositor,
        target: TargetId,
        name: &str,
    ) -> Result<Self, GpuError> {
        let c_name = check_sender_name(name)?;
        let texture = compositor.target_texture(target).ok_or(NO_TARGET)?.clone();
        let device = compositor.device().clone();
        let copied = super::pipeline::query(&device)?;
        // Not OK until the shim says so.
        let mut code: c_int = -1;
        // The open runs Spout's `CleanSenders` (#243: `sdk_cleaning`).
        // SAFETY: `device` is a live ID3D11Device, kept alive until after the
        // shim object is released (see `Shim`); `c_name` is NUL-terminated
        // and outlives the call; `code` is a valid out slot.
        let raw = sdk_cleaning(name, || unsafe {
            spout_sender_open(device.as_raw(), c_name.as_ptr(), &mut code)
        });
        status_result(code, "spout_sender_open", name)?;
        let shim = Shim {
            raw: NonNull::new(raw).ok_or(GpuError::NoObject {
                call: "spout_sender_open",
            })?,
            name: name.to_owned(),
        };
        let listed = listed_now(&shim);
        let raw = shim.raw();
        // SAFETY: `shim` is a live shim sender.
        claim(
            listed,
            || sdk_cleaning(name, || unsafe { spout_sender_claim_name(raw) }),
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
            access_held: 0,
            confirmed_sends: 0,
            device,
            texture,
            copied,
            mode: SpoutSend::default(),
        })
    }

    /// #251: how the next sends hand their frame to Spout.
    pub fn set_send_mode(&mut self, mode: SpoutSend) {
        self.mode = mode;
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
    /// own shared texture on the GPU and announces the frame only once the
    /// GPU has done the copy (#251: the shim's `SendSynced`, never spoutDX's
    /// `SendTexture`, which announced a frame still being written); the
    /// first call registers the sender. [`GpuError::DeviceLost`] means the device must
    /// be rebuilt (and the sender with it). [`GpuError::SpoutNotRegistered`]
    /// means this sender is done (`crate::spout_state`: another sender took
    /// the name, Spout did not list it or could not register it, or its list
    /// stayed unreadable): drop it and make a new one after a backoff. A
    /// [`GpuError::Spout`] is one lost frame.
    ///
    /// Spout skips the copy, and still reports success, when a receiver
    /// holds the sender's mutex for over 67 ms: `send_us` then shows the
    /// wait. #243: a receiver that keeps it for good (handed it abandoned)
    /// gets the sender refused after `spout_state::MAX_ACCESS_HELD` sends,
    /// [`GpuError::SpoutNotRegistered`] naming it.
    pub fn send(&mut self) -> Result<SpoutSendStats, GpuError> {
        let raw = self.shim.raw();
        match before_send(self.registration, || listed_now(&self.shim)) {
            BeforeSend::Send => {}
            BeforeSend::Refused(why) => return Err(self.not_registered(why, None)),
            BeforeSend::Refuse(why) => return Err(self.refuse(why, None)),
        }
        let start = Instant::now();
        // SAFETY: the shim sender is live until Drop; the texture is the
        // compositor's render target, on the device the sender opened on.
        let texture = self.texture.as_raw();
        let done = self.copied.as_raw();
        let bound_ms = super::pipeline::FRAME_WAIT_LIMIT.as_millis() as u64;
        let mode = shim_send_mode(self.mode);
        // #223 follow-up + #251: the shim returns once the GPU has done the
        // copy (and only then announced it), so `send_us` is when Spout's
        // shared texture really holds the frame.
        let code = sdk(&self.name, || unsafe {
            spout_sender_send_synced(raw, texture, done, bound_ms, mode)
        });
        let send_us = micros_since(start);
        unsafe { self.device.GetDeviceRemovedReason() }
            .map_err(|e| GpuError::removed(e.code().0 as u32))?;
        match after_send(self.registration, code, || first_send_facts(&self.shim)) {
            AfterSend::Shared(next) => {
                self.check_texture_lock(send_us)?;
                if next == Registration::Confirmed && self.registration != next {
                    info!(name = %self.name, "sp-gpu: Spout lists the sender");
                }
                let was_confirmed = self.registration == Registration::Confirmed;
                self.registration = next;
                let relisted = was_confirmed && self.relist_if_dropped();
                Ok(SpoutSendStats { send_us, relisted })
            }
            AfterSend::Lost { code, next } => {
                self.check_texture_lock(send_us)?;
                self.registration = next;
                Err(lost_send_error(code, bound_ms))
            }
            AfterSend::Refuse(why) => {
                let holder = self.list_holder(why);
                Err(self.refuse(why, holder))
            }
        }
    }

    /// #243 (SNV 13.10.2026): after a send of a confirmed sender, every
    /// `spout_state::RELIST_EVERY`th one looks at Spout's list and puts the
    /// name back when another program's clean-up dropped it (the same
    /// sender and shared texture: receivers find it again by its name).
    /// Whether it was put back. A failed attempt is tried again at the next
    /// check, 1 s later.
    fn relist_if_dropped(&mut self) -> bool {
        self.confirmed_sends += 1;
        match list_check(self.confirmed_sends, || listed_now(&self.shim)) {
            ListCheck::Skip | ListCheck::Fine => false,
            ListCheck::Relist => {
                let raw = self.shim.raw();
                // SAFETY: the shim sender is live until Drop.
                let code = sdk(&self.name, || unsafe { spout_sender_relist(raw) });
                if code == 0 {
                    warn!(
                        name = %self.name,
                        "sp-gpu: Spout's list had lost the sender (another program's clean-up); listed again"
                    );
                    true
                } else {
                    warn!(
                        name = %self.name,
                        code,
                        "sp-gpu: Spout's list lost the sender and it could not be listed again now; next check in 1 s"
                    );
                    false
                }
            }
        }
    }

    /// #243: after a send that was not refused, whether another program
    /// keeps the sender's texture lock. Such a program was handed the lock
    /// abandoned and never gives it back, so Spout skips every copy and the
    /// receivers show a frozen picture: at `spout_state::MAX_ACCESS_HELD`
    /// sends in a row the sender is refused (named holder, off the list),
    /// and the next sender gets a fresh lock once that program's receiver
    /// has let go.
    fn check_texture_lock(&mut self, send_us: u64) -> Result<(), GpuError> {
        let holder = held_elsewhere(&access_mutex_name(&self.name));
        match access_held(self.access_held, holder.is_some(), send_us) {
            AccessHeld::Streak(streak) => {
                self.access_held = streak;
                Ok(())
            }
            AccessHeld::Refuse(why) => Err(self.refuse(why, holder)),
        }
    }

    /// The size Spout shares, as SpoutDX reports it: (0, 0) before the
    /// first send and once refused, then the render target's size.
    pub fn size(&self) -> (u32, u32) {
        let (mut width, mut height) = (0, 0);
        // SAFETY: the shim sender is live until Drop; both out slots are
        // valid.
        let raw = self.shim.raw();
        sdk(&self.name, || unsafe {
            spout_sender_size(raw, &mut width, &mut height)
        });
        (width, height)
    }

    /// #243: who holds Spout's list, for a refusal because it stayed
    /// unreadable.
    fn list_holder(&self, why: &'static str) -> Option<String> {
        if why == UNREADABLE {
            held_elsewhere(&map_mutex_name(SENDER_NAMES_MAP))
        } else {
            None
        }
    }

    /// Refuse the sender for good: release what it registered. `holder`
    /// names the program that kept the lock the refusal is about.
    fn refuse(&mut self, why: &'static str, holder: Option<String>) -> GpuError {
        // SAFETY: the shim sender is live until Drop.
        let raw = self.shim.raw();
        let code = sdk(&self.name, || unsafe { spout_sender_refuse(raw) });
        if let Err(error) = status_result(code, "spout_sender_refuse", &self.name) {
            warn!(name = %self.name, %error, "sp-gpu: releasing a refused Spout sender failed");
        }
        warn!(name = %self.name, why, holder = ?holder, "sp-gpu: Spout sender refused for good");
        self.registration = Registration::Refused(why);
        self.not_registered(why, holder)
    }

    fn not_registered(&self, why: &'static str, holder: Option<String>) -> GpuError {
        GpuError::SpoutNotRegistered {
            name: self.name.clone(),
            why,
            holder,
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
