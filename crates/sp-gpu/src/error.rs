//! What can go wrong in the compositor, and which failures mean the device
//! must be rebuilt (pure).
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

/// A compositor failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GpuError {
    /// Not Windows: there is no Direct3D 11.
    #[error("the GPU compositor runs on Windows only (Direct3D 11)")]
    Unsupported,
    /// No adapter is a candidate (`pick_adapter`): every one is software or
    /// has no dedicated video memory.
    #[error("no hardware GPU adapter (every adapter is software or has no video memory)")]
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
    /// A shader did not compile; `log` is the compiler's message.
    #[error("the {stage} shader did not compile: {log}")]
    Shader { stage: &'static str, log: String },
    /// The GPU did not finish a frame in time.
    #[error("the GPU did not finish the frame within {0} ms")]
    Timeout(u64),
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

    /// Whether the compositor must be rebuilt before the next frame.
    pub fn is_device_lost(&self) -> bool {
        matches!(self, GpuError::DeviceLost { .. })
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
