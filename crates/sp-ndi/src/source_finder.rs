//! #305 item 3: ONE long-lived NDI source finder, polled for the sources the
//! network lists now — SongPlayer's check that its own `SP-program` is
//! discoverable (`sp-server` `playback/ndi_discover.rs`).
//!
//! [`RealNdiSourceFinder`] creates its `NDIlib_find_create_v2` instance once
//! (`show_local_sources = true`, the default groups) and destroys it on drop;
//! every poll is `NDIlib_find_wait_for_sources` + `NDIlib_find_get_current_sources`
//! on that same instance, never a new finder per poll (a new finder starts a
//! fresh discovery round). The NDI input's own source list
//! (`NdiReceiveBackend::find_source_names`, a short-lived finder every 30 s)
//! is a different, unchanged path.
//!
//! [`NdiSourceFinder`] is the mockable seam; `MockNdiSourceFinder`
//! (`source_finder_mock.rs`, `sp_ndi::test_util`) scripts the list.

use std::ffi::{CStr, c_char};
use std::ptr;
use std::sync::{Arc, Mutex};

use tracing::{info, warn};

use crate::ndi_sdk::NdiLib;
use crate::types::{NDIlib_find_create_t, NDIlib_find_instance_t};

/// One source as the finder lists it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FoundSource {
    /// `"MACHINE (stream)"`.
    pub name: String,
    /// `host:port` (the SDK's `p_url_address`), empty when none.
    pub url: String,
}

/// The sources the network lists now, from one long-lived finder.
pub trait NdiSourceFinder: Send + Sync {
    /// Wait up to `wait_ms` for a change of the list, then return it whole.
    fn current_sources(&self, wait_ms: u32) -> Vec<FoundSource>;
}

/// The finder instance pointer, touched only under the [`Mutex`] (the SDK
/// pointer is not `Send` by itself).
struct FinderPtr(*mut NDIlib_find_instance_t);

// SAFETY: the pointer is used only through the SDK, one call at a time under
// `RealNdiSourceFinder::finder`'s lock, and destroyed once in `Drop`.
unsafe impl Send for FinderPtr {}

/// Production [`NdiSourceFinder`] on the process's one [`NdiLib`].
pub struct RealNdiSourceFinder {
    /// Keeps the library loaded (the find functions point into it).
    lib: Arc<NdiLib>,
    finder: Mutex<FinderPtr>,
}

impl RealNdiSourceFinder {
    /// Create the ONE finder instance; `None` (a WARN) when the SDK returns
    /// no instance.
    #[cfg_attr(test, mutants::skip)] // needs a loaded NDI runtime (never on Linux CI)
    pub fn new(lib: Arc<NdiLib>) -> Option<Self> {
        let create = NDIlib_find_create_t {
            show_local_sources: true,
            p_groups: ptr::null(),
            p_extra_ips: ptr::null(),
        };
        // SAFETY: `create` outlives the call; the instance is destroyed in Drop.
        let finder = unsafe { (lib.find_create_v2)(&create) };
        if finder.is_null() {
            warn!("ndi discover: NDIlib_find_create_v2 returned null — no source finder");
            return None;
        }
        info!("ndi discover: the long-lived NDI source finder is created");
        Some(Self {
            lib,
            finder: Mutex::new(FinderPtr(finder)),
        })
    }
}

/// Copy a possibly-null C string (empty on null).
#[cfg_attr(test, mutants::skip)]
fn owned(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: the SDK's NUL-terminated string, valid until the next call on
    // this finder; copied at once.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

impl NdiSourceFinder for RealNdiSourceFinder {
    #[cfg_attr(test, mutants::skip)] // dereferences the loaded SDK's find functions
    fn current_sources(&self, wait_ms: u32) -> Vec<FoundSource> {
        let finder = self.finder.lock().unwrap_or_else(|p| p.into_inner());
        let mut found = Vec::new();
        // SAFETY: a live instance; the array is owned by the finder and valid
        // until its next call — copied out under the same lock.
        unsafe {
            (self.lib.find_wait_for_sources)(finder.0, wait_ms);
            let mut count: u32 = 0;
            let arr = (self.lib.find_get_current_sources)(finder.0, &mut count);
            if !arr.is_null() {
                for i in 0..count as usize {
                    let s = &*arr.add(i);
                    found.push(FoundSource {
                        name: owned(s.p_ndi_name),
                        url: owned(s.p_url_address),
                    });
                }
            }
        }
        found
    }
}

impl Drop for RealNdiSourceFinder {
    #[cfg_attr(test, mutants::skip)]
    fn drop(&mut self) {
        let finder = self.finder.lock().unwrap_or_else(|p| p.into_inner());
        // SAFETY: created in `new`, destroyed exactly once here.
        unsafe { (self.lib.find_destroy)(finder.0) };
    }
}
