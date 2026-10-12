//! Giving back a Spout lock this thread was handed abandoned, and naming
//! the program that holds one (#243). Why and every decision:
//! `crate::spout_lock` (pure, Linux-tested); this file only calls Win32
//! and ntdll's `NtQueryMutant`.

use std::ffi::c_void;

use tracing::warn;
use windows::Win32::Foundation::{CloseHandle, FALSE, HANDLE};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThreadId, MUTEX_MODIFY_STATE, OpenMutexW, OpenProcess,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    ReleaseMutex, SYNCHRONIZATION_SYNCHRONIZE,
};
use windows::core::{HSTRING, PWSTR};

use crate::spout::map_mutex_name;
use crate::spout_lock::{holder_text, releases_owed, sender_lock_names};

#[link(name = "ntdll", kind = "raw-dylib")]
unsafe extern "system" {
    /// `NtQueryMutant(handle, class, info, length, return length)`: class
    /// 0 = `MUTANT_BASIC_INFORMATION`, 1 = `MUTANT_OWNER_INFORMATION`.
    fn NtQueryMutant(
        handle: HANDLE,
        class: i32,
        info: *mut c_void,
        length: u32,
        returned: *mut u32,
    ) -> i32;
}

/// `MUTANT_BASIC_INFORMATION`.
#[repr(C)]
#[derive(Default)]
struct MutantBasic {
    /// 1 = free, 0 = owned once, −n = owned n + 1 times.
    count: i32,
    owned_by_caller: u8,
    abandoned: u8,
}

/// `MUTANT_OWNER_INFORMATION`: the owner's `CLIENT_ID`, 0/0 when free.
#[repr(C)]
#[derive(Default)]
struct MutantOwner {
    process: usize,
    thread: usize,
}

/// A named mutex opened to query and release it; closed when dropped.
struct Named(HANDLE);

impl Named {
    /// The mutex `name`, if it exists (nothing is created).
    fn open(name: &str) -> Option<Self> {
        // SAFETY: the name outlives the call.
        unsafe {
            OpenMutexW(
                SYNCHRONIZATION_SYNCHRONIZE | MUTEX_MODIFY_STATE,
                FALSE,
                &HSTRING::from(name),
            )
        }
        .ok()
        .map(Self)
    }

    /// One `NtQueryMutant` of class `class` into `T`.
    fn query<T: Default>(&self, class: i32) -> Option<T> {
        let mut info = T::default();
        // SAFETY: `info` is a valid, writable `T` of the size passed, and
        // `self.0` an open mutex handle with query access.
        let status = unsafe {
            NtQueryMutant(
                self.0,
                class,
                (&mut info as *mut T).cast(),
                size_of::<T>() as u32,
                std::ptr::null_mut(),
            )
        };
        (status >= 0).then_some(info)
    }
}

impl Drop for Named {
    fn drop(&mut self) {
        // SAFETY: the handle is this value's own, open, and closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Give back every Spout lock a call of the sender `sender` can take that
/// this thread holds: between SDK calls it holds one only when it was
/// handed it abandoned. Call it on the thread that made the SDK call.
pub(super) fn give_back(sender: &str) {
    for name in sender_lock_names(sender) {
        give_back_one(&name);
    }
}

/// Give back the info-map lock of every sender Spout lists, which Spout's
/// `CleanSenders` takes one by one (at a sender's open).
pub(super) fn give_back_listed() {
    let Ok(listed) = super::spout_sender_names() else {
        return;
    };
    for sender in listed {
        give_back_one(&map_mutex_name(&sender));
    }
}

/// Release the mutex `name` as often as this thread holds it; how many
/// times it did.
fn give_back_one(name: &str) -> u32 {
    let Some(mutex) = Named::open(name) else {
        return 0;
    };
    let Some(basic) = mutex.query::<MutantBasic>(0) else {
        return 0;
    };
    let mut released = 0;
    for _ in 0..releases_owed(basic.count, basic.owned_by_caller != 0) {
        // SAFETY: NtQueryMutant says this thread owns the mutex.
        if unsafe { ReleaseMutex(mutex.0) }.is_err() {
            break;
        }
        released += 1;
    }
    if released > 0 {
        warn!(
            mutex = name,
            released,
            abandoned = basic.abandoned,
            "sp-gpu: gave back a Spout lock this thread was handed abandoned \
             (a program died holding it, #243)"
        );
    }
    released
}

/// Who holds the mutex `name` now, as `holder_text` names it, when a thread
/// other than this one does; `None` when it is free, gone, or this
/// thread's own.
pub(super) fn held_elsewhere(name: &str) -> Option<String> {
    let owner = Named::open(name)?.query::<MutantOwner>(1)?;
    // SAFETY: plain reads of the calling thread's and process's ids.
    let (this_thread, this_process) = unsafe { (GetCurrentThreadId(), GetCurrentProcessId()) };
    let (pid, tid) = (owner.process as u32, owner.thread as u32);
    if tid == 0 || tid == this_thread {
        return None;
    }
    Some(holder_text(pid, image_of(pid).as_deref(), this_process))
}

/// The full path of process `pid`'s image, if it can be read.
fn image_of(pid: u32) -> Option<String> {
    // SAFETY: a plain open; the handle is closed below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) }.ok()?;
    let mut path = [0u16; 1024];
    let mut len = path.len() as u32;
    // SAFETY: `path` holds `len` u16s and outlives the call; `len` is a
    // valid in/out slot.
    let read = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut len,
        )
    };
    // SAFETY: the handle is ours, open, and closed once.
    let _ = unsafe { CloseHandle(process) };
    read.ok()?;
    Some(String::from_utf16_lossy(&path[..len as usize]))
}
