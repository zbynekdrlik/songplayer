//! #330: the GPU scheduling priority of SongPlayer's video DECODE devices.
//!
//! sp-server's video-decode settings task sets it from the setting
//! `video_decode_gpu_priority` ([`set_requested`]); every
//! [`VideoDevice`](crate::VideoDevice) made after that (a `Hardware`
//! reader's own, a #279 producer's) is given it with
//! `IDXGIDevice::SetGPUThreadPriority` and reads it back
//! ([`note_applied`] / [`note_error`]). The compositor's device is never
//! given it: MAX, the FHD sender and the NDI readback stay at DXGI's
//! default. The process value lives here, outside `win/` (the mutation
//! gate's reach), so the bookkeeping is Linux-tested.

use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

static REQUESTED: AtomicI32 = AtomicI32::new(0);
static ERRORS: AtomicU64 = AtomicU64::new(0);
static LAST: Mutex<Option<Applied>> = Mutex::new(None);

/// What the last decode device was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    /// The priority asked for.
    pub requested: i32,
    /// `GetGPUThreadPriority` read back on that device.
    pub read_back: i32,
}

/// The process's decode priority and what the devices made since did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodePriorityStatus {
    /// The priority a decode device made now is given.
    pub requested: i32,
    /// The last decode device's, `None` before the first.
    pub last: Option<Applied>,
    /// Devices whose priority could not be set or read back.
    pub errors: u64,
}

/// Set the priority the next decode devices are given; whether it changed.
pub fn set_requested(priority: i32) -> bool {
    REQUESTED.swap(priority, Ordering::Relaxed) != priority
}

/// The priority a decode device made now is given.
pub fn requested() -> i32 {
    REQUESTED.load(Ordering::Relaxed)
}

/// A decode device was given `requested` and reads `read_back`.
pub fn note_applied(requested: i32, read_back: i32) {
    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = Some(Applied {
        requested,
        read_back,
    });
}

/// A decode device's priority could not be set or read back.
pub fn note_error() {
    ERRORS.fetch_add(1, Ordering::Relaxed);
}

/// The status `GET /api/v1/status` reports.
pub fn status() -> DecodePriorityStatus {
    DecodePriorityStatus {
        requested: requested(),
        last: *LAST.lock().unwrap_or_else(|e| e.into_inner()),
        errors: ERRORS.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
#[path = "decode_priority_tests.rs"]
mod tests;
