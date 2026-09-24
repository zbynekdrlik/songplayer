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
//!  1. [`acquire_slot_for_spawn`] — a process-global `tokio::sync::Semaphore(1)`. At most
//!     ONE heavy child (isolation / mtl / separation) runs process-wide; the
//!     fair FIFO `acquire().await` makes the two workers alternate naturally.
//!  2. [`memory_ok_for`] — a `GlobalMemoryStatusEx` headroom check AT SPAWN,
//!     inside the slot ([`acquire_slot_for_spawn`]: queue first, measure at
//!     spawn; #144 r2): with the permit HELD, both free physical RAM and free
//!     commit must be ≥ [`HEAVY_STEP_MIN_FREE_BYTES`], else the permit is
//!     released and the tick defers with NO backoff, re-queueing next tick. A
//!     pre-slot reading measured the very child the slot serialises away — from
//!     #168 r3 a separation child commits ~9 GB from its first second, so the
//!     lyrics worker never queued and the #162 FIFO alternation was dead.
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
/// [`acquire_slot_for_spawn`] can hand out `'static` owned permits held across the child's
/// `await`.
static HEAVY_SLOT: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(1)));

/// Slot acquisition against an explicit semaphore (fair FIFO) — the shared
/// acquire used by [`acquire_on_checked`] (production, via
/// [`acquire_slot_for_spawn`]) and directly by the slot-serialization tests, so
/// the guarantee is unit-tested with a local `Arc<Semaphore>` (no process-global
/// state, no DB). The permit is held by the returned [`HeavySlotGuard`] until it
/// is dropped — i.e. until the heavy child exits (including through
/// `run_with_wall_abort`: dropping the future drops the guard, releasing the
/// permit, while `kill_on_drop` kills the child). The serialization is proven by
/// `slot_serializes_two_heavy_steps`; the wait-time log carries no asserted
/// behaviour, so mutation is skipped (like `run_with_wall_abort`).
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

/// Returned by [`acquire_slot_for_spawn`] when the spawn-time headroom reading
/// (taken with the permit HELD) is below the floor: the permit is released and
/// the caller defers this heavy step with NO backoff — exactly as the old
/// pre-slot check did — re-queueing behind the running child next tick.
#[derive(Debug)]
pub(crate) struct HeadroomLow;

/// #144 r2 admission: QUEUE for the process-global heavy slot (fair FIFO — the
/// caller blocks behind a running heavy child), THEN measure memory headroom
/// with the permit held; admit only if it passes. Low → drop the permit and
/// return `Err(HeadroomLow)` so the caller defers with NO backoff. This flips the
/// two admission layers (queue first, measure AT SPAWN): a pre-slot reading
/// measured the very child the slot serialises away, so from #168 r3 (a
/// separation child commits ~9 GB from its first second) the lyrics worker never
/// queued and the #162 FIFO alternation was dead. The owner's #162 ruling ("pred
/// ŠTARTOM ťažkého kroku kontrola voľnej pamäte") is this check, at the step's
/// actual start. The real memory read is wired here; the decision is the
/// unit-tested [`acquire_on_checked`].
#[cfg_attr(test, mutants::skip)] // wires the real read_headroom; the decision core is acquire_on_checked
pub(crate) async fn acquire_slot_for_spawn(
    name: &'static str,
) -> Result<HeavySlotGuard, HeadroomLow> {
    acquire_on_checked(HEAVY_SLOT.clone(), name, read_headroom).await
}

/// Injectable core of [`acquire_slot_for_spawn`]: [`acquire_on`] the given
/// semaphore (fair FIFO), then apply [`memory_ok_for`] to the reading from `read`
/// with the permit HELD. Low → drop the guard (releasing the permit) and return
/// `Err(HeadroomLow)`; ok → keep the guard. Linux unit tests drive it with a
/// local `Arc<Semaphore>` + an injected reader — no process-global state, no real
/// memory read (`acquire_for_spawn_*` in `heavy_slot_tests.rs`).
async fn acquire_on_checked(
    slot: Arc<Semaphore>,
    name: &'static str,
    read: impl Fn() -> Option<Headroom>,
) -> Result<HeavySlotGuard, HeadroomLow> {
    let guard = acquire_on(slot, name).await;
    if memory_ok_for(name, read()) {
        Ok(guard)
    } else {
        // Release the permit (drop the guard) so a deferred step never holds the
        // slot — the next FIFO waiter proceeds and this step re-queues next tick.
        drop(guard);
        Err(HeadroomLow)
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

/// The heavy-slot step name the dub's admission uses (`dabing/worker.rs`, via
/// [`acquire_slot_for_spawn`]). The ONE step whose acquire clears [`DUB_SLOT_WANTED`].
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
/// `acquire_slot_for_spawn` (`dabing/worker.rs::process_next`).
pub(crate) fn dub_slot_want_guard() -> DubSlotWant {
    set_dub_slot_wanted(true);
    DubSlotWant
}

/// Test-only serialization for the process-global [`DUB_SLOT_WANTED`] flag —
/// shared by EVERY test in the crate that sets or reads it (heavy_slot flag
/// tests + the stem/lyrics `process_next` tick-defer tests), so parallel test
/// threads never stomp each other's flag reads.
#[cfg(test)]
pub(crate) static DUB_FLAG_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
/// impure input — the live memory read — is injected by [`acquire_on_checked`]
/// (the production reader is [`read_headroom`], via [`acquire_slot_for_spawn`]),
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
static CONTAINMENT: LazyLock<Mutex<Containment>> = LazyLock::new(|| {
    Mutex::new(containment_from_settings(
        None,
        None,
        None,
        None,
        None,
        None,
        logical_cores(),
    ))
});

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
    let c = resolve_containment(pool).await;
    if let Ok(mut g) = CONTAINMENT.lock() {
        *g = c;
    }
    c
}

/// Read every containment setting from the DB and resolve it through the pure
/// [`containment_from_settings`] — WITHOUT publishing (split from
/// [`refresh_containment`] in #147 round 9 so the DB → value path, e.g. a
/// `PATCH /api/v1/settings` of `heavy_max_working_set_mb`, is unit-tested
/// without touching the process-global snapshot other tests read). WARNs on
/// every resolve (each worker tick) while a present value is invalid.
///
/// mutants::skip — DB reads + WARN side effects; the value logic is the pure,
/// mutation-scored `heavy_containment` parse fns.
#[cfg_attr(test, mutants::skip)]
pub(crate) async fn resolve_containment(pool: &sqlx::SqlitePool) -> Containment {
    let cap = crate::db::models::get_setting(pool, "heavy_cpu_cap_pct")
        .await
        .ok()
        .flatten();
    let mask = crate::db::models::get_setting(pool, "heavy_cpu_affinity_mask")
        .await
        .ok()
        .flatten();
    // #207: the operator purge-delay knob, read the same way. A present but
    // out-of-range value collapses to -1 (never decommit); WARN once so the
    // operator sees the setting was ignored (the pure parse fn stays silent).
    let purge = crate::db::models::get_setting(pool, "heavy_purge_delay_ms")
        .await
        .ok()
        .flatten();
    // #207 phase-3: the operator alloc-mode knob (retained | lazy), read the
    // same way. An unrecognised non-empty value collapses to retained; WARN once
    // so the operator sees the setting was ignored (the pure parse fn stays
    // silent).
    let alloc = crate::db::models::get_setting(pool, "heavy_alloc_mode")
        .await
        .ok()
        .flatten();
    // #207 round-3c: the operator reserve-size knob (GiB), read the same way.
    // A present but out-of-range value collapses to the default 4; WARN once
    // so the operator sees the setting was ignored (the pure parse fn stays
    // silent).
    let reserve = crate::db::models::get_setting(pool, "heavy_alloc_reserve_gib")
        .await
        .ok()
        .flatten();
    // #147 round 9: the child's working-set cap (MiB), read the same way; it
    // applies at the NEXT heavy-child spawn (the Job Object is per child).
    let max_ws = crate::db::models::get_setting(pool, "heavy_max_working_set_mb")
        .await
        .ok()
        .flatten();
    let c = containment_from_settings(
        cap.as_deref(),
        mask.as_deref(),
        purge.as_deref(),
        alloc.as_deref(),
        reserve.as_deref(),
        max_ws.as_deref(),
        logical_cores(),
    );
    if crate::lyrics::heavy_containment::max_working_set_setting_ignored(max_ws.as_deref()) {
        warn!(
            "heavy_max_working_set_mb={max_ws:?} is invalid or out of range (0 or 512..=10240 MiB) — using {} MiB",
            c.max_working_set_mb
        );
    }
    if c.purge_delay_ms == -1 && purge.as_deref().is_some_and(|r| r.trim() != "-1") {
        warn!(
            "heavy_purge_delay_ms={purge:?} is invalid or out of range (-1 or 0..=600000 ms) — using -1 (never decommit)"
        );
    }
    if alloc
        .as_deref()
        .is_some_and(|r| !matches!(r.trim(), "" | "retained" | "lazy"))
    {
        warn!("heavy_alloc_mode={alloc:?} is unrecognised (retained or lazy) — using retained");
    }
    if reserve
        .as_deref()
        .is_some_and(|r| !r.trim().parse::<i64>().is_ok_and(|v| (1..=8).contains(&v)))
    {
        warn!(
            "heavy_alloc_reserve_gib={reserve:?} is invalid or out of range (1..=8 GiB) — using {}",
            crate::lyrics::heavy_alloc_env::RESERVE_GIB_DEFAULT
        );
    }
    c
}

/// The currently published containment. Read by the `#[cfg(windows)]` Job
/// Object seam AND (cross-platform, #207) by `stems/separator.rs` to carry the
/// live `purge_delay_ms` into the separation child's mimalloc env. Falls back to
/// the box default if the lock is poisoned.
#[cfg_attr(test, mutants::skip)]
pub(crate) fn current_containment() -> Containment {
    CONTAINMENT.lock().map(|g| *g).unwrap_or_else(|_| {
        containment_from_settings(None, None, None, None, None, None, logical_cores())
    })
}

/// The `heavy child contained (pid …)` INFO line for a spawned child. Pure, so
/// it is exact-string tested cross-platform (#207); emitted once per child by
/// the `#[cfg(windows)]` Job Object seam, so it is dead in the non-Windows lib
/// target. Gains ` alloc_mode=<retained|lazy>` after `purge_delay_ms=` (#207
/// phase-3), ` reserve_gib=<n>` after `alloc_mode=` (#207 round-3c) and
/// ` max_ws_mb=<n|off>` after `reserve_gib=` (#147 round 9 — the working-set cap
/// the Job Object actually APPLIED; `off` = disabled or rejected by the OS).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn contained_line(pid: u32, limit_bytes: usize, c: &Containment) -> String {
    let max_ws = match c.max_working_set_mb {
        0 => "off".to_string(),
        mb => mb.to_string(),
    };
    format!(
        "heavy child contained (pid {pid}): mem_limit={limit_bytes}B cpu_cap={}% affinity=0x{:x} mem_priority_low={} purge_delay_ms={} alloc_mode={} reserve_gib={} max_ws_mb={max_ws}",
        c.cpu_cap_pct,
        c.affinity_mask,
        c.memory_priority_low,
        c.purge_delay_ms,
        c.alloc_mode.as_str(),
        c.reserve_gib,
    )
}

/// The logical-core count of the currently published affinity block — the
/// `count_ones()` of the live containment's mask. Read by the cpu-idle thread
/// cap ([`crate::lyrics::heavy_plan`]) so a heavy child never gets more torch
/// threads than the core block it is confined to (#168 round 5: a 4-logical-core
/// default block caps the #162 quarter-cores rule). Integration-only — reads the
/// published global, like [`current_containment`].
#[cfg_attr(test, mutants::skip)]
pub(crate) fn current_affinity_block_cores() -> usize {
    current_containment().affinity_mask.count_ones() as usize
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
    // #168: the page-fault-rate sampler for this child, aborted when the child
    // exits (this guard drops). Read in Drop, so never `dead_code`.
    #[cfg(windows)]
    sampler: Option<tokio::task::AbortHandle>,
}

#[cfg(windows)]
impl Drop for ChildJobGuard {
    #[cfg_attr(test, mutants::skip)] // closes an OS Job handle — only the kernel can observe it (KILL_ON_JOB_CLOSE); no in-process oracle
    fn drop(&mut self) {
        // #168: stop sampling this child's page faults (it is exiting).
        if let Some(s) = &self.sampler {
            s.abort();
        }
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
    let pid = child.id();
    let handle = match pid {
        Some(pid) => assign_win_job(
            pid,
            CHILD_JOB_MEMORY_LIMIT_BYTES as usize,
            current_containment(),
        ),
        None => None,
    };
    // #168: start logging this child's page-fault rate (`heavy child faults/s`),
    // aborted by the guard's Drop when the child exits.
    let sampler = pid.map(crate::lyrics::heavy_faults::spawn_fault_sampler);
    ChildJobGuard { handle, sampler }
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
    use crate::lyrics::heavy_containment::{
        JOB_LIMIT_AFFINITY, JOB_LIMIT_KILL_ON_JOB_CLOSE, JOB_LIMIT_PROCESS_MEMORY,
        JOB_LIMIT_WORKINGSET, cpu_rate_from_pct, job_limit_flags, job_working_set_bytes,
    };
    use core::ffi::c_void;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE,
        JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP, JOB_OBJECT_LIMIT_AFFINITY,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
        JOB_OBJECT_LIMIT_WORKINGSET, JOBOBJECT_CPU_RATE_CONTROL_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectCpuRateControlInformation,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        MEMORY_PRIORITY_INFORMATION, MEMORY_PRIORITY_LOW, OpenProcess, PROCESS_SET_INFORMATION,
        PROCESS_SET_QUOTA, PROCESS_TERMINATE, ProcessMemoryPriority, SetProcessInformation,
    };
    // The pure flag mirrors (`job_limit_flags`, Linux-tested) must equal the SDK.
    const _: () = assert!(JOB_LIMIT_WORKINGSET == JOB_OBJECT_LIMIT_WORKINGSET);
    const _: () = assert!(JOB_LIMIT_AFFINITY == JOB_OBJECT_LIMIT_AFFINITY);
    const _: () = assert!(JOB_LIMIT_PROCESS_MEMORY == JOB_OBJECT_LIMIT_PROCESS_MEMORY);
    const _: () = assert!(JOB_LIMIT_KILL_ON_JOB_CLOSE == JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE);

    // SAFETY: every handle is null-checked; on any failure we close what we
    // opened and return None. Each struct is zero-initialised then fully set;
    // the CpuRate union write and the info-class SetInformationJobObject calls
    // pass a correctly sized struct as the API requires.
    unsafe {
        let job: HANDLE = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return None;
        }
        // `Err(GetLastError())` read IMMEDIATELY after a failed call (before any
        // logging code can clobber the thread's last-error value).
        let set_extended = |info: &JOBOBJECT_EXTENDED_LIMIT_INFORMATION| {
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                info as *const _ as *const c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                Err(GetLastError())
            } else {
                Ok(())
            }
        };
        // #203: ONE extended-limit struct carries the memory ceiling,
        // kill-on-close AND the core affinity (the wall keeps its cores) — a
        // single SetInformationJobObject call, extending the #162 job. #147
        // round 9 adds the working-set CAP (JOB_OBJECT_LIMIT_WORKINGSET +
        // Minimum/MaximumWorkingSetSize) so the child pages ITSELF instead of
        // evicting SongPlayer; `applied` records what the OS actually took.
        let mut applied = containment;
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = job_limit_flags(applied.max_working_set_mb);
        info.BasicLimitInformation.Affinity = applied.affinity_mask as usize;
        if let Some((min, max)) = job_working_set_bytes(applied.max_working_set_mb) {
            info.BasicLimitInformation.MinimumWorkingSetSize = min;
            info.BasicLimitInformation.MaximumWorkingSetSize = max;
        }
        info.ProcessMemoryLimit = limit_bytes;
        if let Err(err) = set_extended(&info) {
            // #147 round 9: a rejected working-set cap must never cost the
            // #162 memory ceiling + kill-on-close — retry once WITHOUT it.
            if applied.max_working_set_mb == 0 {
                CloseHandle(job);
                return None;
            }
            tracing::warn!(
                "heavy child working-set cap {} MiB rejected (pid {pid}, err {err}) — job applied without it",
                applied.max_working_set_mb
            );
            applied.max_working_set_mb = 0;
            without_working_set(&mut info);
            if set_extended(&info).is_err() {
                CloseHandle(job);
                return None;
            }
        }
        // #203: CPU hard cap — a separate rate-control info class on the same
        // job. Best-effort: a failure keeps the already-applied memory +
        // affinity limits rather than dropping the whole job.
        let mut rate: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION = std::mem::zeroed();
        rate.ControlFlags =
            JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
        rate.Anonymous.CpuRate = cpu_rate_from_pct(applied.cpu_cap_pct);
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
        if applied.memory_priority_low {
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
        let mut assigned = AssignProcessToJobObject(job, proc);
        if assigned == 0 && applied.max_working_set_mb != 0 {
            let err = GetLastError(); // read before any logging call
            // #147 round 9: the job's working-set limits are applied to the
            // process AT assignment — if that is what failed, drop the cap and
            // retry once so the child still gets the memory ceiling + kill-on-close.
            tracing::warn!(
                "heavy child job assignment with a {} MiB working-set cap failed (pid {pid}, err {err}) — retrying without it",
                applied.max_working_set_mb
            );
            applied.max_working_set_mb = 0;
            without_working_set(&mut info);
            if set_extended(&info).is_ok() {
                assigned = AssignProcessToJobObject(job, proc);
            }
        }
        CloseHandle(proc);
        if assigned == 0 {
            CloseHandle(job);
            return None;
        }
        // #203 / #207 / #147 r9: log the APPLIED containment once per child
        // (pure formatter, so the line is exact-string tested cross-platform).
        tracing::info!("{}", contained_line(pid, limit_bytes, &applied));
        Some(job as isize)
    }
}

/// #147 round 9: clear the working-set cap from a heavy child's extended-limit
/// struct (flags back to the #162/#203 set, both sizes zero) for the
/// cap-rejected retry. Integration-only.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
fn without_working_set(
    info: &mut windows_sys::Win32::System::JobObjects::JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
) {
    info.BasicLimitInformation.LimitFlags = crate::lyrics::heavy_containment::job_limit_flags(0);
    info.BasicLimitInformation.MinimumWorkingSetSize = 0;
    info.BasicLimitInformation.MaximumWorkingSetSize = 0;
}

#[cfg(test)]
#[path = "heavy_slot_tests.rs"]
mod tests;
