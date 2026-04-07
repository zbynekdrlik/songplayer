//! Dynamic loader for the NDI SDK shared library.
//!
//! Uses `libloading` to open the NDI runtime at process start and resolve
//! the sender function pointers we need. No link-time dependency on NDI.

use libloading::{Library, Symbol};
use tracing::{debug, error, info};

use crate::error::NdiError;
use crate::types::{
    NDIlib_audio_frame_v3_t, NDIlib_send_create_t, NDIlib_send_instance_t, NDIlib_tally_t,
    NDIlib_video_frame_v2_t,
};

// ---------------------------------------------------------------------------
// Function-pointer type aliases (match the NDI SDK C signatures)
// ---------------------------------------------------------------------------

type FnInitialize = unsafe extern "C" fn() -> bool;
type FnDestroy = unsafe extern "C" fn();
type FnSendCreate =
    unsafe extern "C" fn(*const NDIlib_send_create_t) -> *mut NDIlib_send_instance_t;
type FnSendDestroy = unsafe extern "C" fn(*mut NDIlib_send_instance_t);
type FnSendVideoV2 =
    unsafe extern "C" fn(*mut NDIlib_send_instance_t, *const NDIlib_video_frame_v2_t);
type FnSendAudioV3 =
    unsafe extern "C" fn(*mut NDIlib_send_instance_t, *const NDIlib_audio_frame_v3_t);
type FnSendGetTally =
    unsafe extern "C" fn(*mut NDIlib_send_instance_t, *mut NDIlib_tally_t, u32) -> bool;

// ---------------------------------------------------------------------------
// NdiLib — owns the library handle and resolved function pointers
// ---------------------------------------------------------------------------

/// Loaded NDI SDK with resolved sender function pointers.
///
/// The library is unloaded (and `NDIlib_destroy` called) on [`Drop`].
#[allow(missing_debug_implementations)]
pub struct NdiLib {
    // Keep the library alive so function pointers remain valid.
    _library: Library,
    #[allow(dead_code)]
    pub(crate) initialize: FnInitialize,
    pub(crate) destroy: FnDestroy,
    pub(crate) send_create: FnSendCreate,
    pub(crate) send_destroy: FnSendDestroy,
    pub(crate) send_send_video_v2: FnSendVideoV2,
    pub(crate) send_send_audio_v3: FnSendAudioV3,
    pub(crate) send_get_tally: FnSendGetTally,
}

// SAFETY: The function pointers are loaded from a shared library and are
// valid for the lifetime of `_library`. NDI SDK functions are thread-safe
// when used on different sender instances.
unsafe impl Send for NdiLib {}
unsafe impl Sync for NdiLib {}

impl NdiLib {
    /// Attempt to load the NDI runtime library and resolve all required symbols.
    ///
    /// Search order:
    /// 1. `NDI_RUNTIME_DIR_V6` environment variable
    /// 2. `NDI_RUNTIME_DIR_V5` environment variable
    /// 3. System library search path (default `libloading` behaviour)
    ///
    /// After loading, calls `NDIlib_initialize()`. Returns [`NdiError::InitFailed`]
    /// if that returns false.
    pub fn load() -> Result<Self, NdiError> {
        let library = Self::open_library()?;

        // SAFETY: we resolve well-known NDI SDK symbols whose signatures are
        // defined by the NDI SDK C headers.
        unsafe {
            let initialize = Self::resolve::<FnInitialize>(&library, b"NDIlib_initialize\0")?;
            let destroy = Self::resolve::<FnDestroy>(&library, b"NDIlib_destroy\0")?;
            let send_create = Self::resolve::<FnSendCreate>(&library, b"NDIlib_send_create\0")?;
            let send_destroy = Self::resolve::<FnSendDestroy>(&library, b"NDIlib_send_destroy\0")?;
            let send_send_video_v2 =
                Self::resolve::<FnSendVideoV2>(&library, b"NDIlib_send_send_video_v2\0")?;
            let send_send_audio_v3 =
                Self::resolve::<FnSendAudioV3>(&library, b"NDIlib_send_send_audio_v3\0")?;
            let send_get_tally =
                Self::resolve::<FnSendGetTally>(&library, b"NDIlib_send_get_tally\0")?;

            // Call NDIlib_initialize — required before any other NDI call.
            info!("Calling NDIlib_initialize()");
            if !(initialize)() {
                error!("NDIlib_initialize() returned false");
                return Err(NdiError::InitFailed);
            }
            info!("NDI SDK initialized successfully");

            Ok(Self {
                _library: library,
                initialize,
                destroy,
                send_create,
                send_destroy,
                send_send_video_v2,
                send_send_audio_v3,
                send_get_tally,
            })
        }
    }

    /// Try to open the NDI shared library from known locations.
    fn open_library() -> Result<Library, NdiError> {
        let candidates = Self::library_candidates();
        let mut last_err = String::new();

        for path in &candidates {
            debug!("Trying NDI library: {path}");
            match unsafe { Library::new(path) } {
                Ok(lib) => {
                    info!("Loaded NDI library from: {path}");
                    return Ok(lib);
                }
                Err(e) => {
                    debug!("Failed to load {path}: {e}");
                    last_err = format!("{path}: {e}");
                }
            }
        }

        Err(NdiError::LibraryNotFound(last_err))
    }

    /// Build a list of candidate library paths to try.
    fn library_candidates() -> Vec<String> {
        let mut candidates = Vec::new();

        // Environment-variable paths first.
        for env_var in ["NDI_RUNTIME_DIR_V6", "NDI_RUNTIME_DIR_V5"] {
            if let Ok(dir) = std::env::var(env_var) {
                let lib_name = Self::platform_lib_name();
                let full = format!("{dir}/{lib_name}");
                // On Windows backslash is fine too.
                let full_win = format!("{dir}\\{lib_name}");
                candidates.push(full);
                if cfg!(windows) {
                    candidates.push(full_win);
                }
            }
        }

        // Fallback: bare library name — relies on system search path.
        if cfg!(windows) {
            candidates.push("Processing.NDI.Lib.x64.dll".to_string());
        } else {
            candidates.push("libndi.so.6".to_string());
            candidates.push("libndi.so.5".to_string());
            candidates.push("libndi.so".to_string());
        }

        candidates
    }

    /// Platform-specific library file name.
    fn platform_lib_name() -> &'static str {
        if cfg!(windows) {
            "Processing.NDI.Lib.x64.dll"
        } else {
            "libndi.so.6"
        }
    }

    /// Resolve a single symbol from the loaded library.
    ///
    /// # Safety
    /// Caller must ensure `T` matches the actual symbol's signature.
    unsafe fn resolve<T: Copy>(library: &Library, name: &[u8]) -> Result<T, NdiError> {
        let symbol_name = String::from_utf8_lossy(&name[..name.len() - 1]); // strip NUL for display
        let sym: Symbol<'_, T> = unsafe {
            library
                .get(name)
                .map_err(|_| NdiError::SymbolNotFound(symbol_name.to_string()))?
        };
        Ok(*sym)
    }
}

impl Drop for NdiLib {
    fn drop(&mut self) {
        info!("Calling NDIlib_destroy()");
        // SAFETY: we successfully called initialize, so we must call destroy.
        unsafe {
            (self.destroy)();
        }
    }
}
