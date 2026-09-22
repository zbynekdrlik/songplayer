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
//!     OOM kills the CHILD, never the host. It also carries the #203 containment
//!     ([`refresh_containment`] / [`current_containment`]): a CPU rate-control
//!     HARD cap, `JOB_OBJECT_LIMIT_AFFINITY` to the upper cores, and the child's
//!     `MEMORY_PRIORITY_LOW` — so a heavy child can never starve the wall.
//!
//! The pure decision core ([`headroom_ok`] / [`admission_from_headroom`] /
//! [`memory_ok_for`]) is unit-tested with an INJECTED [`Headroom`] — the real
//! read ([`read_headroom`]) and the Job Object ([`assign_child_job`]) are
//! `#[cfg(windows)]` integration seams (no-ops off Windows).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{info, warn};

use crate::lyrics::heavy_containment::{Containment, containment_from_settings};

/// Free physical RAM AND free commit each required at or above this before any
/// heavy child spawns. 4 GiB clears one ~3.2 GB CPU-RoFormer working set with
/// margin on the 16 GB box, so a heavy step never starts the box into a
/// low-virtual-memory condition (the 07:40 crash).
pub(crate) const HEAVY_STEP_MIN_FREE_BYTES: u64 = 4_294_967_296; // 4 GiB

/// Per-child Windows Job Object memory ceiling. Above the single-child working
/// set (~3.2 GB) with headroom, so a genuine runaway is killed at the CHILD,
/// never by starving the host. Only referenced inside the `#[cfg(windows)]`
/// Job Object path.
///
/// Raised from 6 GiB to 10 GiB (2026-09-15, live finding on win-resolume
/// 10:19–10:45 UTC): 6 GiB is fine for a normal ≤4-minute song (measured
/// identical throughput inside vs. outside a 6 GiB job), but a 10-minute
/// "warm-up" file ("10 Minute Daily Vocal Workout", 118 MB FLAC) pins private
/// bytes at ~5.0 GB against the cap — CUDA context reservation plus several
/// float32 copies of the whole mix — so allocations start failing and the
/// child crawls at 0.2 cores with ~200k page faults/s for 20+ minutes before
/// timing out. 10 GiB clears that working set with margin. Paired with the
/// stems worker's own `STEM_MAX_DURATION_MS` cap (`stems/worker.rs`), which
/// skips separation only past 120 minutes (a multi-hour livestream) — this
/// ceiling is for the
/// lyrics-worker heavy steps (isolation / mtl), which have no duration cap.
// Platform-independent literal (no arithmetic: the cfg(windows) product was
// invisible to the Linux mutation runner) — pinned by `child_job_limit_is_ten_gib`.
#[cfg_attr(not(windows), allow(dead_code))] // only the Windows Job Object path reads it
pub(crate) const CHILD_JOB_MEMORY_LIMIT_BYTES: u64 = 10_737_418_240; // 10 GiB

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
    // #184 G0.1: the dub step's acquire means a queued dub now HOLDS the slot —
    // clear the "dub wants the slot" flag so the stem/lyrics workers stop
    // yielding for it (the semaphore serialises the rest). The pure, unit-tested
    // `acquire_clears_dub_want` gates it so no other step touches the flag.
    if acquire_clears_dub_want(name) {
        set_dub_slot_wanted(false);
    }
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
// #184 G0.1 — dub-priority flag. A dub job about to take the heavy slot sets
// this TRUE (via `dub_slot_want_guard`) so a running stem separation yields the
// slot (the stem worker's mid-run watcher checks it) and the stem/lyrics workers
// defer their next heavy tick; it is cleared the instant the dub step acquires
// the slot (in `acquire_on`), so it means precisely "a dub is queued behind the
// slot".
// ---------------------------------------------------------------------------

/// The heavy-slot step name the dub child's `acquire_slot` uses
/// (`dabing/child.rs`). The ONE step whose acquire clears [`DUB_SLOT_WANTED`].
pub(crate) const DUB_STEP_NAME: &str = "dub live-translate";

/// Process-global "a dub job is queued behind the heavy slot" flag.
static DUB_SLOT_WANTED: AtomicBool = AtomicBool::new(false);

/// Whether a dub job is currently queued behind the heavy slot — read by the
/// stem worker (tick-defer + mid-run yield) and the lyrics worker (tick-defer).
pub(crate) fn dub_slot_wanted() -> bool {
    DUB_SLOT_WANTED.load(Ordering::Relaxed)
}

/// Publish the "a dub is queued behind the slot" flag.
pub(crate) fn set_dub_slot_wanted(wanted: bool) {
    DUB_SLOT_WANTED.store(wanted, Ordering::Relaxed);
}

/// Pure: whether acquiring the heavy slot under step `name` should clear
/// [`DUB_SLOT_WANTED`]. TRUE only for the dub step — its acquire means the dub
/// now HOLDS the slot, so nothing needs to yield for it any more (the semaphore
/// serialises the rest). Unit-tested exactly so the name coupling cannot drift.
pub(crate) fn acquire_clears_dub_want(name: &str) -> bool {
    name == DUB_STEP_NAME
}

/// RAII "a dub wants the heavy slot" signal from [`dub_slot_want_guard`]: its
/// Drop clears [`DUB_SLOT_WANTED`] — the early-return safety net for every path
/// out of the dub `synthesize` before the acquire (the acquire itself clears the
/// flag the instant it succeeds, in [`acquire_on`], so the flag means precisely
/// "queued, not yet acquired").
///
/// INVARIANT: the flag is a bool, not a refcount, and Drop clears it
/// UNCONDITIONALLY — safe only because dub synthesis is STRICTLY SERIAL (one
/// `DubWorker::run` loop awaits `synthesize` fully before the next tick, so at
/// most one guard exists at a time). If a second concurrent dub `synthesize` is
/// ever introduced, one guard's Drop would clear another's still-queued want —
/// make the flag a counter (or key it per-dub) BEFORE going concurrent.
pub(crate) struct DubSlotWant;

impl Drop for DubSlotWant {
    fn drop(&mut self) {
        set_dub_slot_wanted(false);
    }
}

/// Publish "a dub is queued behind the heavy slot" (flag TRUE) and return the
/// RAII guard whose Drop clears it. Created immediately before the dub's
/// `acquire_slot` (`dabing/worker.rs::synthesize`).
pub(crate) fn dub_slot_want_guard() -> DubSlotWant {
    set_dub_slot_wanted(true);
    DubSlotWant
}

/// Test-only serialization for the process-global [`DUB_SLOT_WANTED`] flag —
/// shared by EVERY test in the crate that sets or reads it (heavy_slot flag
/// tests + the stem/lyrics `process_next` tick-defer tests), so parallel test
/// threads never stomp each other's flag reads.
#[cfg(test)]
pub(crate) static DUB_FLAG_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

/// Positive-form twin of [`heavy_step_memory_ok`] for call sites: `true` when
/// the heavy step must be deferred this tick. Call sites use this instead of
/// `!heavy_step_memory_ok(..)` so no `!` sits at the spawn seam (a deleted-`!`
/// mutant there is unobservable without real memory pressure); the decision
/// itself is the unit-tested `memory_ok_for`.
#[cfg_attr(test, mutants::skip)] // thin negation over a real-memory read; the core is tested via memory_ok_for
pub(crate) fn heavy_step_memory_defers(name: &str) -> bool {
    !heavy_step_memory_ok(name)
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
// #203 — per-child OS containment (CPU hard cap + core affinity + memory
// priority). The pure decision core is [`crate::lyrics::heavy_containment`]; the
// live settings + core count are read here (like the #162 kill-switches) and
// published to a process-global snapshot the Job Object seam reads at every
// heavy-child spawn.
// ---------------------------------------------------------------------------

/// The live containment published by the heavy workers each tick, read by the
/// Job Object seam ([`assign_child_job`]) at every heavy-child spawn. Initialised
/// to the box's defaults (no override) so a spawn before the first refresh is
/// still contained.
static CONTAINMENT: LazyLock<Mutex<Containment>> =
    LazyLock::new(|| Mutex::new(containment_from_settings(None, None, logical_cores())));

/// The box's logical-processor count (`available_parallelism`, min 1). Reads the
/// environment, so integration-only; the pure mask/cap rules it feeds are tested.
/// Shared with the `/api/v1/status` handler so both resolve containment from the
/// SAME core count.
#[cfg_attr(test, mutants::skip)]
pub(crate) fn logical_cores() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Resolve the live [`Containment`] from the `heavy_cpu_cap_pct` /
/// `heavy_cpu_affinity_mask` settings + the box's core count and publish it for
/// the next heavy child. Each heavy worker (stems / lyrics / dub) calls this each
/// tick, exactly like it reads the kill-switches — so a dashboard change takes
/// effect on the next child spawned, no restart. Returns the resolved value.
#[cfg_attr(test, mutants::skip)]
pub(crate) async fn refresh_containment(pool: &sqlx::SqlitePool) -> Containment {
    let cap = crate::db::models::get_setting(pool, "heavy_cpu_cap_pct")
        .await
        .ok()
        .flatten();
    let mask = crate::db::models::get_setting(pool, "heavy_cpu_affinity_mask")
        .await
        .ok()
        .flatten();
    let c = containment_from_settings(cap.as_deref(), mask.as_deref(), logical_cores());
    if let Ok(mut g) = CONTAINMENT.lock() {
        *g = c;
    }
    c
}

/// The currently published containment (applied by the Job Object seam). Read
/// only inside the `#[cfg(windows)]` spawn path; falls back to the box default if
/// the lock is poisoned.
#[cfg_attr(not(windows), allow(dead_code))]
#[cfg_attr(test, mutants::skip)]
fn current_containment() -> Containment {
    CONTAINMENT
        .lock()
        .map(|g| *g)
        .unwrap_or_else(|_| containment_from_settings(None, None, logical_cores()))
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
    #[cfg_attr(test, mutants::skip)] // closes an OS Job handle — only the kernel can observe it (KILL_ON_JOB_CLOSE); no in-process oracle
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

/// Put a freshly-spawned heavy `child` under a Job Object with the
/// [`CHILD_JOB_MEMORY_LIMIT_BYTES`] ceiling (an OOM kills the child, not the
/// host) PLUS the #203 containment currently published by the workers
/// ([`current_containment`]): the CPU hard cap, the core affinity, and the
/// child's lowered memory priority. Best-effort: any failure logs and returns a
/// no-op guard (the child still has `kill_on_drop` as its own net). Non-Windows:
/// a no-op guard.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
pub(crate) fn assign_child_job(child: &tokio::process::Child) -> ChildJobGuard {
    let handle = match child.id() {
        Some(pid) => assign_win_job(
            pid,
            CHILD_JOB_MEMORY_LIMIT_BYTES as usize,
            current_containment(),
        ),
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
fn assign_win_job(pid: u32, limit_bytes: usize, containment: Containment) -> Option<isize> {
    use crate::lyrics::heavy_containment::cpu_rate_from_pct;
    use core::ffi::c_void;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE,
        JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP, JOB_OBJECT_LIMIT_AFFINITY,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
        JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectCpuRateControlInformation, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        MEMORY_PRIORITY_INFORMATION, MEMORY_PRIORITY_LOW, OpenProcess, PROCESS_SET_INFORMATION,
        PROCESS_SET_QUOTA, PROCESS_TERMINATE, ProcessMemoryPriority, SetProcessInformation,
    };

    // SAFETY: every handle is null-checked; on any failure we close what we
    // opened and return None. Each struct is zero-initialised then fully set;
    // the CpuRate union write and the info-class SetInformationJobObject calls
    // pass a correctly sized struct as the API requires.
    unsafe {
        let job: HANDLE = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return None;
        }
        // #203: ONE extended-limit struct carries the memory ceiling,
        // kill-on-close AND the core affinity (the wall keeps its cores) — a
        // single SetInformationJobObject call, extending the #162 job.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY
            | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_AFFINITY;
        info.BasicLimitInformation.Affinity = containment.affinity_mask as usize;
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
        // #203: CPU hard cap — a separate rate-control info class on the same
        // job. Best-effort: a failure keeps the already-applied memory +
        // affinity limits rather than dropping the whole job.
        let mut rate: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION = std::mem::zeroed();
        rate.ControlFlags =
            JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
        rate.Anonymous.CpuRate = cpu_rate_from_pct(containment.cpu_cap_pct);
        if SetInformationJobObject(
            job,
            JobObjectCpuRateControlInformation,
            &rate as *const _ as *const c_void,
            std::mem::size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
        ) == 0
        {
            tracing::warn!("heavy child CPU rate cap not applied (pid {pid})");
        }
        let proc: HANDLE = OpenProcess(
            PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_SET_INFORMATION,
            0,
            pid,
        );
        if proc.is_null() {
            CloseHandle(job);
            return None;
        }
        // #203: lower the child's process MEMORY priority so the wall's working
        // set is never trimmed for it. Best-effort (needs PROCESS_SET_INFORMATION).
        if containment.memory_priority_low {
            let mem = MEMORY_PRIORITY_INFORMATION {
                MemoryPriority: MEMORY_PRIORITY_LOW,
            };
            if SetProcessInformation(
                proc,
                ProcessMemoryPriority,
                &mem as *const _ as *const c_void,
                std::mem::size_of::<MEMORY_PRIORITY_INFORMATION>() as u32,
            ) == 0
            {
                tracing::warn!("heavy child memory priority not lowered (pid {pid})");
            }
        }
        let assigned = AssignProcessToJobObject(job, proc);
        CloseHandle(proc);
        if assigned == 0 {
            CloseHandle(job);
            return None;
        }
        // #203: log the applied containment once per child.
        tracing::info!(
            "heavy child contained (pid {pid}): mem_limit={limit_bytes}B cpu_cap={}% affinity=0x{:x} mem_priority_low={}",
            containment.cpu_cap_pct,
            containment.affinity_mask,
            containment.memory_priority_low
        );
        Some(job as isize)
    }
}

#[cfg(test)]
#[path = "heavy_slot_tests.rs"]
mod tests;
