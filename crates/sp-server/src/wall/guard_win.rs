//! #254: the wall guard's ONE act on the box (Windows): ending a crashed or
//! hung Resolume Arena that `guard::decide` ordered, its crash reporter's
//! processes first, then Arena, so the operator's relaunch script brings it
//! back. The owner's exception to "the wall report acts on nothing"
//! (ROZHODNUTÉ 14.10.2026, #254 comment 6169316862): nothing else on the
//! box is touched here — no setting, no display, no clip.
//!
//! Each process is opened by its id and checked before it is ended: its
//! image must be the one ordered (`BugSplatMonitor.exe`, `Arena.exe`), and
//! Arena's start time the one read (a process id is reused). A process
//! that is gone already (`ERROR_INVALID_PARAMETER`) counts as ended. Not
//! compiled on the Linux runner, so every fn is `mutants::skip`; every
//! decision is `guard.rs`'s, tested on Linux.

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, GetLastError, HANDLE};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    QueryFullProcessImageNameW, TerminateProcess,
};

use super::guard::{self, CRASH_MONITOR_IMAGE, EndOrder};
use super::read::ARENA_IMAGE;

/// The exit code an ended process gets.
const END_EXIT_CODE: u32 = 1;

/// The longest image path read, in UTF-16 units.
const PATH_UNITS: usize = 1024;

/// Closes a process handle when dropped.
struct Opened(HANDLE);

impl Drop for Opened {
    #[cfg_attr(test, mutants::skip)] // Windows glue
    fn drop(&mut self) {
        // SAFETY: the handle was opened by `open` and is closed once, here.
        unsafe { CloseHandle(self.0) };
    }
}

/// Ends `order`'s crash reporter's processes, then its Arena, each only
/// while it is the process read (image and start time); why any of them
/// could not be ended, every reason joined.
#[cfg_attr(test, mutants::skip)] // Windows glue
pub fn end(order: &EndOrder) -> Result<(), String> {
    let mut failed = Vec::new();
    for monitor in &order.monitors {
        if let Err(why) = end_process(monitor.pid, CRASH_MONITOR_IMAGE, None) {
            failed.push(why);
        }
    }
    if let Err(why) = end_process(order.pid, ARENA_IMAGE, Some(order.created_100ns)) {
        failed.push(why);
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(failed.join("; "))
    }
}

/// Ends process `pid` when its image is `image` and it was started at
/// `created_100ns`; a process that is gone already is ended.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn end_process(pid: u32, image: &str, created_100ns: Option<u64>) -> Result<(), String> {
    // SAFETY: opens the process to read its image and times and to end it;
    // closed by `Opened`.
    let handle = unsafe {
        OpenProcess(
            PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )
    };
    if handle.is_null() {
        // SAFETY: reads this thread's last error.
        let error = unsafe { GetLastError() };
        if error == ERROR_INVALID_PARAMETER {
            return Ok(()); // no such process: it is gone already
        }
        return Err(format!("OpenProcess({pid}) failed (error {error})"));
    }
    let opened = Opened(handle);
    let path = image_path(&opened, pid)?;
    if !guard::is_image(&path, image) {
        return Err(format!(
            "process {pid} is {path}, not {image}: left running"
        ));
    }
    if let Some(created) = created_100ns {
        let times = super::win::process_times(opened.0, pid)?;
        if times.created_100ns != created {
            return Err(format!(
                "process {pid} is another {image} (started since the read): left running"
            ));
        }
    }
    // SAFETY: `opened` holds the process with PROCESS_TERMINATE.
    if unsafe { TerminateProcess(opened.0, END_EXIT_CODE) } == 0 {
        // SAFETY: reads this thread's last error.
        let error = unsafe { GetLastError() };
        return Err(format!(
            "TerminateProcess({pid}, {image}) failed (error {error})"
        ));
    }
    Ok(())
}

/// The full path of the image of the process `opened` holds.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn image_path(opened: &Opened, pid: u32) -> Result<String, String> {
    let mut path = [0_u16; PATH_UNITS];
    let mut len = PATH_UNITS as u32;
    // SAFETY: `path` holds `len` units; the call writes at most that many and
    // answers the length written (no NUL counted).
    let read = unsafe {
        QueryFullProcessImageNameW(opened.0, PROCESS_NAME_WIN32, path.as_mut_ptr(), &mut len)
    };
    if read == 0 {
        // SAFETY: reads this thread's last error.
        let error = unsafe { GetLastError() };
        return Err(format!(
            "QueryFullProcessImageNameW({pid}) failed (error {error})"
        ));
    }
    let len = (len as usize).min(PATH_UNITS);
    Ok(String::from_utf16_lossy(&path[..len]))
}
