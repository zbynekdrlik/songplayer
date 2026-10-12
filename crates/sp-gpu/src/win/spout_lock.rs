//! Giving back a Spout lock this thread was handed abandoned, and naming
//! the program that holds one (#243). #243 follow-up: reading who holds
//! Spout's sender list ([`spout_list_holder`]), and the sentinel that is
//! handed it when that program exits ([`ListSentinel`]). Why and every
//! decision: `crate::spout_lock` (pure, Linux-tested); this file only calls
//! Win32 and ntdll's `NtQueryMutant`.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

use tracing::warn;
use windows::Win32::Foundation::{CloseHandle, FALSE, HANDLE};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThreadId, MUTEX_MODIFY_STATE, OpenMutexW, OpenProcess,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    ReleaseMutex, SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject,
};
use windows::core::{HSTRING, PWSTR};

use crate::error::GpuError;
use crate::spout::{SENDER_NAMES_MAP, map_mutex_name};
use crate::spout_lock::{
    LockHolder, SENTINEL_WAIT_MS, SentinelEnd, SentinelStep, holder_text, program_name,
    releases_owed, sender_lock_names, sentinel_step,
};

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
    /// Part of the layout only: a wait that takes an abandoned mutex clears
    /// it, so it reads 0 for every mutex this thread owns.
    #[allow(dead_code)]
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

/// The senders Spout lists now (none when the list cannot be read: our own
/// reader gives back an abandoned list lock it meets, `read_map`).
pub(super) fn listed_senders() -> Vec<String> {
    super::spout_sender_names().unwrap_or_default()
}

/// Give back the info-map lock of each of `senders`, which Spout's
/// `CleanSenders` takes one by one.
pub(super) fn give_back_maps<'a>(senders: impl IntoIterator<Item = &'a String>) {
    for sender in senders {
        give_back_one(&map_mutex_name(sender));
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
            "sp-gpu: gave back a Spout lock this thread was handed abandoned \
             (a program died holding it, #243)"
        );
    }
    released
}

/// Who holds the mutex `name` now, as `holder_text` names it, when a thread
/// other than this one does (another program's, or another of this
/// process's: then ", this process"); `None` when it is free, gone, or
/// this thread's own.
pub(super) fn held_elsewhere(name: &str) -> Option<String> {
    let holder = mutex_holder(name)?;
    // SAFETY: a plain read of the calling process's id.
    let this_process = unsafe { GetCurrentProcessId() };
    Some(holder_text(
        holder.pid,
        holder.image.as_deref(),
        this_process,
    ))
}

/// #243 follow-up: the program whose thread holds the mutex `name` now,
/// when a thread other than the calling one does (another program's, or
/// another thread of this process); `None` when it is free, gone, or the
/// calling thread's own. Doc-hidden: the tests read their own mutex.
#[doc(hidden)]
pub fn mutex_holder(name: &str) -> Option<LockHolder> {
    let owner = Named::open(name)?.query::<MutantOwner>(1)?;
    // SAFETY: a plain read of the calling thread's id.
    let this_thread = unsafe { GetCurrentThreadId() };
    let (pid, tid) = (owner.process as u32, owner.thread as u32);
    if tid == 0 || tid == this_thread {
        return None;
    }
    let image = image_of(pid);
    Some(LockHolder {
        pid,
        image: program_name(image.as_deref()).map(str::to_owned),
    })
}

/// #243 follow-up: the program holding Spout's sender list
/// (`SpoutSenderNames_mutex`) now, as [`mutex_holder`] reads it.
pub fn spout_list_holder() -> Result<Option<LockHolder>, GpuError> {
    Ok(mutex_holder(&map_mutex_name(SENDER_NAMES_MAP)))
}

/// #243 follow-up: a thread queued on Spout's sender list
/// (`SpoutSenderNames_mutex`) while another program keeps it. When that
/// program exits, Windows hands the lock to its next waiter — this thread,
/// queued before any program started later (a relaunched Arena) — which
/// gives it back at once and ends. It waits in bounded waits
/// ([`SENTINEL_WAIT_MS`]) and looks at its stop flag after each: it never
/// spins and never holds the lock past the release. Every step is
/// `crate::spout_lock::sentinel_step`'s.
pub struct ListSentinel {
    stop: Arc<AtomicBool>,
    ended: mpsc::Receiver<SentinelEnd>,
    thread: Option<JoinHandle<()>>,
}

impl ListSentinel {
    /// Start waiting on Spout's sender list.
    pub fn start() -> Result<Self, GpuError> {
        Self::on(&map_mutex_name(SENDER_NAMES_MAP))
    }

    /// The sentinel on the mutex `name`. Doc-hidden: tests wait on their
    /// own mutex (Spout's list is machine-wide).
    #[doc(hidden)]
    pub fn on(name: &str) -> Result<Self, GpuError> {
        let name = name.to_owned();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let (report, ended) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("spout-list-sentinel".into())
            .spawn(move || {
                let _ = report.send(wait_and_give_back(&name, &flag));
            })
            .map_err(|e| GpuError::Thread(format!("the Spout list sentinel did not start: {e}")))?;
        Ok(Self {
            stop,
            ended,
            thread: Some(thread),
        })
    }

    /// How it ended, once it has (each end is returned once).
    pub fn ended(&self) -> Option<SentinelEnd> {
        self.ended.try_recv().ok()
    }

    /// Stop it and wait until it ended: at most one wait. A lock it was
    /// handed in that wait is given back first.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ListSentinel {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The sentinel's thread: wait on the mutex `name` until it is handed the
/// lock (given back at once), asked to stop, or a wait fails.
fn wait_and_give_back(name: &str, stop: &AtomicBool) -> SentinelEnd {
    let Some(mutex) = Named::open(name) else {
        return SentinelEnd::Failed(format!("the mutex {name} did not open"));
    };
    loop {
        // SAFETY: `mutex.0` is an open mutex handle with SYNCHRONIZE access.
        let wait = unsafe { WaitForSingleObject(mutex.0, SENTINEL_WAIT_MS) }.0;
        match sentinel_step(wait, stop.load(Ordering::SeqCst)) {
            SentinelStep::GiveBack { abandoned } => {
                // SAFETY: the wait just gave this thread the mutex.
                return match unsafe { ReleaseMutex(mutex.0) } {
                    Ok(()) => SentinelEnd::GivenBack { abandoned },
                    Err(e) => SentinelEnd::Failed(format!("ReleaseMutex: {e}")),
                };
            }
            SentinelStep::WaitAgain => {}
            SentinelStep::Stop => return SentinelEnd::Stopped,
            SentinelStep::Fail => {
                return SentinelEnd::Failed(format!("the wait answered {wait:#x}"));
            }
        }
    }
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
