//! #196: process-start instant, surfaced as `/api/v1/status.uptime_s`.
//!
//! The post-deploy E2E job reads `uptime_s` to SKIP restarting a SongPlayer the
//! Deploy job started less than 10 min ago (item 6 — halve the restarts per
//! push). Marked at the top of `lib::start`, read by the status handler.

use std::sync::OnceLock;
use std::time::Instant;

// #147 round 9: the pure hard-minimum-working-set core (parse / clamp / plan /
// the outcome line), Linux-tested; the Win32 calls stay in this file.
#[path = "process_residency.rs"]
pub mod residency;

static START: OnceLock<Instant> = OnceLock::new();

/// Record the process start. Idempotent — only the FIRST call is kept, so the
/// uptime is measured from the earliest `start()` in the process.
///
/// mutants::skip — a `OnceLock` set with no observable return; its effect
/// (`uptime_secs`) is a wall-clock read that no terminating unit test can pin.
#[cfg_attr(test, mutants::skip)]
pub fn mark_started() {
    let _ = START.set(Instant::now());
}

/// Seconds since the process started, or `0` if `mark_started` was never called
/// (e.g. a unit test that never boots the server).
///
/// mutants::skip — a wall-clock elapsed read; only catchable by a wall-time
/// assertion (non-deterministic on the runner). The DECISION it feeds lives in
/// the CI shell (skip restart iff deployed version AND `uptime_s < 600`).
#[cfg_attr(test, mutants::skip)]
pub fn uptime_secs() -> u64 {
    START.get().map(|t| t.elapsed().as_secs()).unwrap_or(0)
}

/// #203: raise SongPlayer to `HIGH_PRIORITY_CLASS` at startup so the NDI SDK's
/// own compression threads pre-empt the contained heavy children (stems / lyrics
/// / dub), which run `BELOW_NORMAL` under a Job Object CPU cap + affinity. NOT
/// `REALTIME` — that starves the OS input/paging threads. Best-effort + logged;
/// a no-op off Windows.
///
/// mutants::skip — a one-shot OS scheduling-class call with no in-process oracle
/// (only the kernel scheduler can observe it); the box read verifies it live.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
pub fn set_high_priority_class() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, HIGH_PRIORITY_CLASS, SetPriorityClass,
    };
    // SAFETY: GetCurrentProcess returns the current-process pseudo-handle;
    // SetPriorityClass on it only changes this process's scheduling class.
    let ok = unsafe { SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS) };
    if ok == 0 {
        tracing::warn!("could not set SongPlayer HIGH_PRIORITY_CLASS (#203)");
    } else {
        tracing::info!("SongPlayer priority class set to HIGH (#203)");
    }
}

/// Non-Windows: no priority class to set (the box is Windows-only prod).
#[cfg(not(windows))]
pub fn set_high_priority_class() {}

/// The priority-class label for `/api/v1/status.heavy_containment.priority_class`:
/// `"high"` where [`set_high_priority_class`] applies (Windows), else `"default"`.
///
/// mutants::skip — a cfg-derived platform string with no cross-platform value
/// assertion possible (asserting `"high"` fails on the Linux job, `"default"` on
/// the Windows job — the #189 platform-string trap).
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
pub fn priority_class_label() -> &'static str {
    "high"
}

/// Non-Windows twin — SongPlayer keeps the default class off the box.
#[cfg(not(windows))]
#[cfg_attr(test, mutants::skip)]
pub fn priority_class_label() -> &'static str {
    "default"
}

/// #147 round 9: read `sp_min_working_set_mb` — the SAME key
/// `PATCH /api/v1/settings` writes (the settings API stores any key) — and
/// resolve it through the pure parse (absent / garbage / negative → the
/// 3072 MiB default, `0` = disabled, clamped `256..=8192`). WARNs once when a
/// present value was not used as written.
///
/// mutants::skip — a DB read + a WARN side effect; the value logic is the pure
/// `residency::parse_min_working_set_mb` (mutation-scored), and this path is
/// still exercised end-to-end by `resolve_min_working_set_reads_the_patched_setting`.
#[cfg_attr(test, mutants::skip)]
pub async fn resolve_min_working_set_mb(pool: &sqlx::SqlitePool) -> u32 {
    let raw = crate::db::models::get_setting(pool, residency::SETTING_KEY)
        .await
        .ok()
        .flatten();
    let mb = residency::parse_min_working_set_mb(raw.as_deref());
    if residency::min_working_set_setting_ignored(raw.as_deref()) {
        tracing::warn!(
            "{}={raw:?} is invalid or out of range (0 or {}..={} MiB) — using {mb} MiB",
            residency::SETTING_KEY,
            residency::SP_MIN_WS_FLOOR_MB,
            residency::SP_MIN_WS_CEIL_MB,
        );
    }
    mb
}

/// #147 round 9: give SongPlayer a HARD minimum working set so a heavy child's
/// growth can never trim the paced sender's frame pools / NDI SDK buffers
/// (genlock pacing is ON in production permanently). Called once from
/// `lib.rs::start()` right after the DB is ready — before any pipeline spawns —
/// so a change to `sp_min_working_set_mb` takes effect at the NEXT start.
/// Best-effort: the outcome is ONE INFO line (`sp working set: … privilege=…
/// result=ok|failed(err=<GetLastError>)`), never a panic. Off Windows the
/// setting is still resolved + logged, with no OS call.
///
/// mutants::skip — the OS call has no in-process oracle; the plan + the line
/// are pure and tested in `process_residency_tests.rs`.
#[cfg_attr(test, mutants::skip)]
pub async fn apply_min_working_set(pool: &sqlx::SqlitePool) {
    let mb = resolve_min_working_set_mb(pool).await;
    let plan = residency::plan_hard_min(mb);
    let (privilege, set) = match plan.as_ref() {
        Some(p) => set_hard_min_working_set(p),
        None => (Ok(()), Ok(())),
    };
    tracing::info!(
        "{}",
        residency::residency_line(plan.as_ref(), privilege, set)
    );
}

/// The `(privilege, set)` outcomes of the hard-minimum call, each
/// `Err(GetLastError())` on failure.
type WorkingSetOutcome = (Result<(), u32>, Result<(), u32>);

/// Enable `SeIncreaseWorkingSetPrivilege` (needed to raise the minimum above the
/// current working set — granted to normal users, but DISABLED in the token by
/// default), then `SetProcessWorkingSetSizeEx(GetCurrentProcess(), min, max,
/// HARDWS_MIN_ENABLE | HARDWS_MAX_DISABLE)`. The quota call is attempted even
/// when the privilege step fails (an elevated token, or a minimum at or below
/// the current working set, does not need it).
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
fn set_hard_min_working_set(plan: &residency::WorkingSetPlan) -> WorkingSetOutcome {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::System::Memory::{
        QUOTA_LIMITS_HARDWS_MAX_DISABLE, QUOTA_LIMITS_HARDWS_MIN_ENABLE, SetProcessWorkingSetSizeEx,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // The pure flag mirrors in `residency` must equal the SDK constants.
    const _: () = assert!(residency::QUOTA_HARDWS_MIN_ENABLE == QUOTA_LIMITS_HARDWS_MIN_ENABLE);
    const _: () = assert!(residency::QUOTA_HARDWS_MAX_DISABLE == QUOTA_LIMITS_HARDWS_MAX_DISABLE);

    let privilege = enable_increase_working_set_privilege();
    // SAFETY: GetCurrentProcess returns the current-process pseudo-handle (full
    // access, never closed); SetProcessWorkingSetSizeEx only changes this
    // process's working-set quota and returns 0 on failure, after which
    // GetLastError reads the calling thread's error code.
    let set = unsafe {
        if SetProcessWorkingSetSizeEx(
            GetCurrentProcess(),
            plan.min_bytes,
            plan.max_bytes,
            plan.flags,
        ) == 0
        {
            Err(GetLastError())
        } else {
            Ok(())
        }
    };
    (privilege, set)
}

/// Non-Windows: there is no working-set quota to set (prod is Windows-only).
#[cfg(not(windows))]
fn set_hard_min_working_set(_plan: &residency::WorkingSetPlan) -> WorkingSetOutcome {
    (Ok(()), Ok(()))
}

/// Enable `SeIncreaseWorkingSetPrivilege` in this process's token.
/// `AdjustTokenPrivileges` returns success even when the token does not hold
/// the privilege, setting `ERROR_NOT_ALL_ASSIGNED` — reported as a failure too.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
fn enable_increase_working_set_privilege() -> Result<(), u32> {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_NOT_ALL_ASSIGNED, GetLastError, HANDLE, LUID,
    };
    use windows_sys::Win32::Security::{
        AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_INC_WORKING_SET_NAME,
        SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: the token handle is checked and closed on every path after it
    // opens; `luid` / `tp` are fully initialised PODs passed by pointer as the
    // APIs expect; no previous-state buffer is requested (null + length 0).
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        ) == 0
        {
            return Err(GetLastError());
        }
        let mut luid: LUID = std::mem::zeroed();
        if LookupPrivilegeValueW(std::ptr::null(), SE_INC_WORKING_SET_NAME, &mut luid) == 0 {
            let e = GetLastError();
            CloseHandle(token);
            return Err(e);
        }
        let tp = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        let ok =
            AdjustTokenPrivileges(token, 0, &tp, 0, std::ptr::null_mut(), std::ptr::null_mut());
        let err = GetLastError();
        CloseHandle(token);
        if ok == 0 || err == ERROR_NOT_ALL_ASSIGNED {
            Err(err)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #147 r9: the startup resolver reads the SAME key `PATCH /api/v1/settings`
    /// writes (the settings API stores any key), so a PATCH takes effect at the
    /// next start. Absent → the 3072 MiB default; `0` disables; out of range
    /// clamps; garbage falls back to the default.
    #[tokio::test]
    async fn resolve_min_working_set_reads_the_patched_setting() {
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        assert_eq!(resolve_min_working_set_mb(&pool).await, 3072, "absent");
        for (value, want) in [("4096", 4096), ("0", 0), ("99999", 8192), ("junk", 3072)] {
            crate::db::models::set_setting(&pool, "sp_min_working_set_mb", value)
                .await
                .unwrap();
            assert_eq!(resolve_min_working_set_mb(&pool).await, want, "{value}");
        }
    }

    /// The off-Windows twin makes no OS call and reports both steps ok, so the
    /// startup line on a dev box never claims a failure that did not happen.
    #[cfg(not(windows))]
    #[test]
    fn non_windows_hard_min_is_a_no_op() {
        let plan = residency::plan_hard_min(3072).unwrap();
        assert_eq!(set_hard_min_working_set(&plan), (Ok(()), Ok(())));
    }
}
