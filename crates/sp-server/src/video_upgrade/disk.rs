//! #223 S12a: the free space of the cache's volume (the worker's disk
//! floor, `worker::MIN_FREE_BYTES`).

use std::path::Path;

/// The bytes free to this process on `dir`'s volume, `None` when the OS did
/// not answer.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)] // one OS call; the floor is `worker::decide`, tested
pub(crate) fn free_bytes(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free: u64 = 0;
    // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call;
    // the two totals it may skip are null, which the API allows.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(free)
}

/// Off Windows nothing is read (the upgrade's reader is Windows only).
#[cfg(not(windows))]
#[cfg_attr(test, mutants::skip)] // a constant
pub(crate) fn free_bytes(_dir: &Path) -> Option<u64> {
    None
}
