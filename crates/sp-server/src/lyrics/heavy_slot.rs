//! #162 — box-wide "background processing must never overload the PC" guard.
//!
//! Owner ruling (2026-09-15, verbatim): *"spracovanie na pozadí je VŽDY
//! sekvenčné … nikdy paralelne, a nikdy nesmie preťažiť PC"* — the goal is
//! MINIMAL load, not speed. Before this, the lyrics worker (vocal isolation +
//! mtl align) and the stem worker (separation) were two independent loops with
//! NO shared heavy-step lock; once #162 removed the idle-only gate they each
//! spawned a ~3.2 GB CPU RoFormer child in the same second on the 16 GB box →
//! Windows low-virtual-memory → SongPlayer abort `0xc0000409` + OBS died
//! (2026-09-15 07:40 UTC, dump `dumps\SongPlayer.exe.4892.dmp`).
//!
//! Three layers, all funnelled through EVERY heavy child spawn:
//!  1. [`acquire_slot`] — a process-global `tokio::sync::Semaphore(1)`. At most
//!     ONE heavy child (isolation / mtl / separation) runs process-wide; the
//!     fair FIFO `acquire().await` makes the two workers alternate naturally.
//!  2. [`heavy_step_memory_ok`] — a `GlobalMemoryStatusEx` headroom check
//!     BEFORE acquiring the slot (owner's order): both free physical RAM and
//!     free commit must be ≥ [`HEAVY_STEP_MIN_FREE_BYTES`], else the tick defers
//!     with NO backoff and re-checks next tick.
//!  3. [`assign_child_job`] — a per-child Windows Job Object with a
//!     `JOB_OBJECT_LIMIT_PROCESS_MEMORY` ceiling + `KILL_ON_JOB_CLOSE`, so an
//!     OOM kills the CHILD, never the host.
//!
//! The pure decision core ([`headroom_ok`] / [`admission_from_headroom`] /
//! [`memory_ok_for`]) is unit-tested with an INJECTED [`Headroom`] — the real
//! read ([`read_headroom`]) and the Job Object ([`assign_child_job`]) are
//! `#[cfg(windows)]` integration seams (no-ops off Windows).

use std::sync::{Arc, LazyLock};
use std::time::Instant;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{info, warn};

/// Free physical RAM AND free commit each required at or above this before any
/// heavy child spawns. 4 GiB clears one ~3.2 GB CPU-RoFormer working set with
/// margin on the 16 GB box, so a heavy step never starts the box into a
/// low-virtual-memory condition (the 07:40 crash).
pub(crate) const HEAVY_STEP_MIN_FREE_BYTES: u64 = 0; // RED (#162): GREEN sets 4 GiB

/// Per-child Windows Job Object memory ceiling. Above the single-child working
/// set (~3.2 GB) with headroom, so a genuine runaway is killed at the CHILD,
/// never by starving the host. Only referenced inside the `#[cfg(windows)]`
/// Job Object path.
#[cfg(windows)]
const CHILD_JOB_MEMORY_LIMIT_BYTES: usize = 6 * 1024 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Layer 1 — process-global heavy-step slot (Semaphore with ONE permit).
// ---------------------------------------------------------------------------

/// The one process-wide heavy-step permit. `LazyLock<Arc<..>>` so
/// [`acquire_slot`] can hand out `'static` owned permits held across the child's
/// `await`.
static HEAVY_SLOT: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(1)));

/// Acquire the process-global heavy-step permit (fair FIFO). Held by the
/// returned [`HeavySlotGuard`] until it is dropped — i.e. until the heavy child
/// exits (including through `run_with_wall_abort`: dropping the future drops the
/// guard, releasing the permit, while `kill_on_drop` kills the child). Logs the
/// wait time on acquire and a `released` line on drop.
pub(crate) async fn acquire_slot(name: &'static str) -> HeavySlotGuard {
    acquire_on(HEAVY_SLOT.clone(), name).await
}

/// Slot acquisition against an explicit semaphore — the injectable core of
/// [`acquire_slot`], so the serialization guarantee is unit-tested with a local
/// `Arc<Semaphore>` (no process-global state, no DB). The serialization is
/// proven by `slot_serializes_two_heavy_steps`; the wait-time log carries no
/// asserted behaviour, so mutation is skipped (like `run_with_wall_abort`).
#[cfg_attr(test, mutants::skip)]
async fn acquire_on(slot: Arc<Semaphore>, name: &'static str) -> HeavySlotGuard {
    let start = Instant::now();
    let permit = slot
        .acquire_owned()
        .await
        .expect("heavy-step slot semaphore is never closed");
    let waited_ms = start.elapsed().as_millis();
    info!("heavy step {name} slot acquired (waited {waited_ms} ms)");
    HeavySlotGuard {
        _permit: permit,
        name,
    }
}

/// RAII hold on the heavy-step permit. Dropping it releases the permit (and logs
/// `released`), so the next waiting heavy step proceeds.
pub(crate) struct HeavySlotGuard {
    // Held only for its Drop (releasing the permit); never read, so `dead_code`
    // would flag it under the workspace's `-D warnings`.
    #[allow(dead_code)]
    _permit: OwnedSemaphorePermit,
    name: &'static str,
}

impl Drop for HeavySlotGuard {
    #[cfg_attr(test, mutants::skip)] // log-only; the permit release is the OwnedSemaphorePermit's own Drop
    fn drop(&mut self) {
        info!("heavy step {} slot released", self.name);
    }
}

// ---------------------------------------------------------------------------
// Layer 2 — memory-headroom guard (pure core + Windows read).
// ---------------------------------------------------------------------------

/// A free-memory reading: free physical RAM and free commit, both in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Headroom {
    pub(crate) free_phys: u64,
    pub(crate) free_commit: u64,
}

/// Pure: enough headroom to start a heavy step? BOTH free physical and free
/// commit must be at or above `min`.
pub(crate) fn headroom_ok(free_phys: u64, free_commit: u64, min: u64) -> bool {
    free_phys >= min && free_commit >= min
}

/// Whether a heavy step may proceed given a memory reading. `None` (memory
/// unreadable, or non-Windows) is treated as `Ok` — an unknown reading must
/// never wedge the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemoryAdmission {
    Ok,
    Defer { free_phys: u64, free_commit: u64 },
}

/// Pure: map a headroom reading to an admission decision against
/// [`HEAVY_STEP_MIN_FREE_BYTES`].
pub(crate) fn admission_from_headroom(reading: Option<Headroom>) -> MemoryAdmission {
    match reading {
        None => MemoryAdmission::Ok,
        Some(h) if headroom_ok(h.free_phys, h.free_commit, HEAVY_STEP_MIN_FREE_BYTES) => {
            MemoryAdmission::Ok
        }
        Some(h) => MemoryAdmission::Defer {
            free_phys: h.free_phys,
            free_commit: h.free_commit,
        },
    }
}

/// Pure worker-facing gate: `true` = enough headroom to start the heavy step
/// named `name`; `false` = defer (a WARN with the numbers is logged). The only
/// impure input — the live memory read — is injected by [`heavy_step_memory_ok`],
/// so BOTH workers' deferral behaviour is unit-tested with a fed [`Headroom`],
/// never real memory.
pub(crate) fn memory_ok_for(name: &str, reading: Option<Headroom>) -> bool {
    match admission_from_headroom(reading) {
        MemoryAdmission::Ok => true,
        MemoryAdmission::Defer {
            free_phys,
            free_commit,
        } => {
            warn!(
                "heavy step {name} deferred — memory headroom low \
                 (free_phys={free_phys}, free_commit={free_commit})"
            );
            false
        }
    }
}

/// Live worker gate: read the box's free RAM/commit and decide. `true` = start
/// the heavy step, `false` = defer this tick (no backoff). Integration-only
/// (reads real memory); the decision it delegates to ([`memory_ok_for`]) is
/// unit-tested.
#[cfg_attr(test, mutants::skip)]
pub(crate) fn heavy_step_memory_ok(name: &str) -> bool {
    memory_ok_for(name, read_headroom())
}

/// Read free physical RAM + free commit via `GlobalMemoryStatusEx`. `None` on
/// any failure (treated as "allow"). Integration-only.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
fn read_headroom() -> Option<Headroom> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: `status` is a correctly sized, zero-initialised MEMORYSTATUSEX with
    // its `dwLength` set as the API requires; GlobalMemoryStatusEx only writes
    // into it and returns 0 on failure.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return None;
    }
    Some(Headroom {
        free_phys: status.ullAvailPhys,
        free_commit: status.ullAvailPageFile,
    })
}

/// Non-Windows: memory is unreadable here (the box is Windows-only prod) → allow.
#[cfg(not(windows))]
fn read_headroom() -> Option<Headroom> {
    None
}

// ---------------------------------------------------------------------------
// Layer 3 — per-child Windows Job Object (memory ceiling, kill-on-job-close).
// ---------------------------------------------------------------------------

/// Holds a heavy child's Job Object for the child's lifetime. On Windows,
/// dropping it closes the job handle; with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
/// that terminates a still-running child (an error/panic path), complementing
/// the child's own `kill_on_drop(true)`. Non-Windows: a no-op unit.
///
/// The handle is stored as `isize` (not the raw `HANDLE` pointer) so the guard
/// is `Send` and can be held across the child's `await` in a spawned worker
/// future.
pub(crate) struct ChildJobGuard {
    #[cfg(windows)]
    handle: Option<isize>,
}

#[cfg(windows)]
impl Drop for ChildJobGuard {
    fn drop(&mut self) {
        if let Some(h) = self.handle {
            use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
            // SAFETY: `h` is a Job Object handle we created and still solely own;
            // closing it (KILL_ON_JOB_CLOSE) terminates a still-running child,
            // else it is a harmless no-op after the child already exited.
            unsafe {
                CloseHandle(h as HANDLE);
            }
        }
    }
}

/// Put a freshly-spawned heavy `child` under a Job Object capped at
/// [`CHILD_JOB_MEMORY_LIMIT_BYTES`] so an OOM kills the child, not the host.
/// Best-effort: any failure logs at DEBUG and returns a no-op guard (the child
/// still has `kill_on_drop` as its own net). Non-Windows: a no-op guard.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
pub(crate) fn assign_child_job(child: &tokio::process::Child) -> ChildJobGuard {
    let handle = match child.id() {
        Some(pid) => assign_win_job(pid, CHILD_JOB_MEMORY_LIMIT_BYTES),
        None => None,
    };
    ChildJobGuard { handle }
}

/// Non-Windows: no Job Object, no-op guard.
#[cfg(not(windows))]
#[cfg_attr(test, mutants::skip)]
pub(crate) fn assign_child_job(_child: &tokio::process::Child) -> ChildJobGuard {
    ChildJobGuard {}
}

/// Create a Job Object with a process-memory limit + kill-on-job-close, assign
/// process `pid` to it, and return the job HANDLE (as `isize`) to hold for the
/// child's lifetime. `None` on any failure (handles closed). Integration-only.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
fn assign_win_job(pid: u32, limit_bytes: usize) -> Option<isize> {
    use core::ffi::c_void;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    // SAFETY: every handle is null-checked; on any failure we close what we
    // opened and return None. The struct is zero-initialised then fully set.
    unsafe {
        let job: HANDLE = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return None;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_PROCESS_MEMORY | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        info.ProcessMemoryLimit = limit_bytes;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            CloseHandle(job);
            return None;
        }
        let proc: HANDLE = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if proc.is_null() {
            CloseHandle(job);
            return None;
        }
        let assigned = AssignProcessToJobObject(job, proc);
        CloseHandle(proc);
        if assigned == 0 {
            CloseHandle(job);
            return None;
        }
        tracing::debug!("heavy step child job memory limit set to {limit_bytes} bytes (pid {pid})");
        Some(job as isize)
    }
}

#[cfg(test)]
#[path = "heavy_slot_tests.rs"]
mod tests;
