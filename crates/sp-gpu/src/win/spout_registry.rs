//! Reading Spout's registry the way every receiver does (#223 S1b): open
//! the named shared-memory map, take its named mutex (`<map>_mutex`, 67 ms,
//! as `SpoutSharedMemory::Lock`), copy it, let go. The parsing is
//! `crate::spout`'s, tested on Linux; this file only calls Win32.

use std::ffi::{CStr, CString};

use windows::Win32::Foundation::{CloseHandle, FALSE, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows::Win32::System::Memory::{
    FILE_MAP_READ, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    OpenFileMappingA, UnmapViewOfFile, VirtualQuery,
};
use windows::Win32::System::Threading::{CreateMutexA, ReleaseMutex, WaitForSingleObject};
use windows::core::{Error, PCSTR};

use super::failed;
use crate::error::GpuError;
use crate::spout::{
    SENDER_NAMES_MAP, SharedTextureInfo, check_sender_name, is_not_found, map_mutex_name,
    parse_sender_names,
};

/// How long a reader waits for a map's mutex: `SpoutSharedMemory::Lock`'s
/// 67 ms (4 frames at 60 fps).
const LOCK_MS: u32 = 67;

/// A handle closed when dropped.
struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the handle is this value's own, open, and closed once.
        // A failed close leaks one handle; there is nothing to do about it.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// A mapped view, unmapped when dropped.
struct View(MEMORY_MAPPED_VIEW_ADDRESS);

impl Drop for View {
    fn drop(&mut self) {
        // SAFETY: the view is this value's own, mapped, and unmapped once.
        let _ = unsafe { UnmapViewOfFile(self.0) };
    }
}

/// The names Spout's sender list holds, in its order. Empty when no list
/// exists (no Spout program has run since the last one ended).
pub fn spout_sender_names() -> Result<Vec<String>, GpuError> {
    let name = CString::new(SENDER_NAMES_MAP).expect("a constant without NUL");
    Ok(read_map(&name)?
        .map(|map| parse_sender_names(&map))
        .unwrap_or_default())
}

/// The registry entry of the sender `name`: `None` when no sender of that
/// name exists (its map is gone). A name Spout cannot carry is
/// [`GpuError::SpoutName`].
pub fn spout_sender_info(name: &str) -> Result<Option<SharedTextureInfo>, GpuError> {
    let name = check_sender_name(name)?;
    let Some(map) = read_map(&name)? else {
        return Ok(None);
    };
    SharedTextureInfo::parse(&map)
        .map(Some)
        .ok_or(GpuError::NoObject {
            call: "a sender's map shorter than SharedTextureInfo",
        })
}

/// A copy of the whole shared-memory map `name`, taken under its mutex;
/// `None` when the map does not exist.
fn read_map(name: &CStr) -> Result<Option<Vec<u8>>, GpuError> {
    // SAFETY: `name` is NUL-terminated and outlives the call.
    let map = match unsafe { OpenFileMappingA(FILE_MAP_READ.0, FALSE, PCSTR(name.as_ptr().cast())) }
    {
        Ok(map) => Owned(map),
        Err(e) if is_not_found(e.code().0 as u32) => return Ok(None),
        Err(e) => return Err(failed("OpenFileMappingA", &e)),
    };
    // SAFETY: `map` is an open mapping handle; 0 bytes maps all of it.
    let view = unsafe { MapViewOfFile(map.0, FILE_MAP_READ, 0, 0, 0) };
    if view.Value.is_null() {
        return Err(failed("MapViewOfFile", &Error::from_win32()));
    }
    let view = View(view);
    let mut region = MEMORY_BASIC_INFORMATION::default();
    // SAFETY: `region` is a valid out slot of the size passed.
    let got = unsafe {
        VirtualQuery(
            Some(view.0.Value.cast_const()),
            &mut region,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if got == 0 {
        return Err(failed("VirtualQuery", &Error::from_win32()));
    }
    let mutex_name =
        CString::new(map_mutex_name(&name.to_string_lossy())).expect("a name without NUL");
    // SAFETY: the name is NUL-terminated and outlives the call; Spout opens
    // the mutex the same way (CreateMutexA opens an existing one).
    let mutex = unsafe { CreateMutexA(None, FALSE, PCSTR(mutex_name.as_ptr().cast())) }
        .map_err(|e| failed("CreateMutexA", &e))?;
    let mutex = Owned(mutex);
    // SAFETY: `mutex` is an open mutex handle.
    let wait = unsafe { WaitForSingleObject(mutex.0, LOCK_MS) };
    if wait != WAIT_OBJECT_0 {
        if wait == WAIT_ABANDONED {
            // An abandoned mutex is owned now: give it back. Spout's own
            // Lock refuses the map in this case too.
            // SAFETY: this thread owns the mutex.
            let _ = unsafe { ReleaseMutex(mutex.0) };
        }
        return Err(GpuError::Spout {
            call: "WaitForSingleObject (a Spout map's mutex)",
            code: wait.0,
        });
    }
    // SAFETY: the view is mapped for `RegionSize` bytes from its base
    // (`VirtualQuery` of the view), readable, and stays mapped until `view`
    // drops after this copy.
    let bytes = unsafe {
        std::slice::from_raw_parts(view.0.Value.cast::<u8>().cast_const(), region.RegionSize)
    }
    .to_vec();
    // SAFETY: this thread owns the mutex (the wait above).
    unsafe { ReleaseMutex(mutex.0) }.map_err(|e| failed("ReleaseMutex", &e))?;
    Ok(Some(bytes))
}
