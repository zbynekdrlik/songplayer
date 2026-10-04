//! What can go wrong in the compositor and its Spout sender, and which
//! failures mean the device must be rebuilt (pure).
//!
//! #223 R3-2: "Device lost: the device is rebuilt on the next boundary, and
//! the rebuild is counted" — that is S2's `program-max` thread. The
//! compositor's part is to say so: every Direct3D failure is classified by
//! its HRESULT, and the DXGI device-removed family is
//! [`GpuError::DeviceLost`].

use crate::picture::PictureError;

/// The GPU was removed (driver update, TDR, unplug).
pub const DXGI_ERROR_DEVICE_REMOVED: u32 = 0x887A_0005;
/// The GPU hung on badly formed commands.
pub const DXGI_ERROR_DEVICE_HUNG: u32 = 0x887A_0006;
/// The GPU was reset by another device's bad commands.
pub const DXGI_ERROR_DEVICE_RESET: u32 = 0x887A_0007;
/// The driver failed internally.
pub const DXGI_ERROR_DRIVER_INTERNAL_ERROR: u32 = 0x887A_0020;

/// Whether `hresult` means the device is gone and must be rebuilt.
pub fn is_device_lost(hresult: u32) -> bool {
    matches!(
        hresult,
        DXGI_ERROR_DEVICE_REMOVED
            | DXGI_ERROR_DEVICE_HUNG
            | DXGI_ERROR_DEVICE_RESET
            | DXGI_ERROR_DRIVER_INTERNAL_ERROR
    )
}

/// A compositor or Spout sender failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GpuError {
    /// Not Windows: there is no Direct3D 11.
    #[error("the GPU compositor runs on Windows only (Direct3D 11)")]
    Unsupported,
    /// No adapter to compose on: `pick_adapter` found no candidate (every
    /// adapter is software or has no dedicated video memory), or the test
    /// constructor's index is past DXGI's list.
    #[error(
        "no GPU adapter to compose on (none is a hardware adapter with video memory, or the index is past DXGI's list)"
    )]
    NoAdapter,
    /// A picture that is not whole NV12: nothing was uploaded or drawn.
    #[error("invalid NV12 picture: {0}")]
    Picture(#[from] PictureError),
    /// The device is gone (DXGI's device-removed family): rebuild the
    /// compositor.
    #[error("{call}: the GPU device was lost (HRESULT {hresult:#010x})")]
    DeviceLost { call: &'static str, hresult: u32 },
    /// Any other Direct3D / DXGI failure.
    #[error("{call} failed (HRESULT {hresult:#010x})")]
    Api { call: &'static str, hresult: u32 },
    /// A create call reported success but handed back no object (or a
    /// texture slot the draw needs is empty): an API or compositor bug.
    #[error("{call} gave no object")]
    NoObject { call: &'static str },
    /// A shader did not compile; `log` is the compiler's message.
    #[error("the {stage} shader did not compile: {log}")]
    Shader { stage: &'static str, log: String },
    /// The GPU did not finish a frame in time.
    #[error("the GPU did not finish the frame within {0} ms")]
    Timeout(u64),
    /// A Spout sender name Spout cannot carry (`check_sender_name`):
    /// nothing reached Spout.
    #[error("invalid Spout sender name {name:?}: {reason}")]
    SpoutName { name: String, reason: &'static str },
    /// A live sender holds the name. Spout would rename this one
    /// (`<name>_1`), which Arena's `SPOUT_<name>` layer would never show,
    /// so it is refused.
    #[error("the Spout sender name {name:?} is held by another sender")]
    SpoutNameTaken { name: String },
    /// At its first send Spout registered the sender under another name (a
    /// sender took the name in between) or did not list it (its sender list
    /// is full). The registration is released and the sender never sends
    /// again: drop it.
    #[error(
        "Spout did not register the sender as {name:?} (another sender took the name, or Spout's sender list is full)"
    )]
    SpoutNotRegistered { name: String },
    /// A Spout call failed. `code` is the shim's status (3 = the SDK
    /// reported failure, 4 = a C++ exception, 5 = a bad argument) or, for a
    /// registry read, the mutex wait's result.
    #[error("{call} failed in Spout (code {code:#x})")]
    Spout { call: &'static str, code: u32 },
}

impl GpuError {
    /// The error of a failed `call`: [`GpuError::DeviceLost`] for the
    /// device-removed family ([`is_device_lost`]), else [`GpuError::Api`].
    pub fn from_hresult(call: &'static str, hresult: u32) -> Self {
        if is_device_lost(hresult) {
            GpuError::DeviceLost { call, hresult }
        } else {
            GpuError::Api { call, hresult }
        }
    }

    /// The error of a non-OK `ID3D11Device::GetDeviceRemovedReason`: ANY
    /// reason it reports means the device is gone — including
    /// `DXGI_ERROR_INVALID_CALL` (the app's bad call removed it), which
    /// [`GpuError::from_hresult`] would call an ordinary failure.
    pub fn removed(hresult: u32) -> Self {
        GpuError::DeviceLost {
            call: "GetDeviceRemovedReason",
            hresult,
        }
    }

    /// Whether the compositor must be rebuilt before the next frame.
    pub fn is_device_lost(&self) -> bool {
        matches!(self, GpuError::DeviceLost { .. })
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
