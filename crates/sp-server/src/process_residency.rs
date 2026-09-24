//! #147 round 9 — SongPlayer's HARD minimum working set (pure decision core).
//!
//! Genlock pacing is ON in production permanently (owner ruling, issue #147
//! comment 5812898277). The residual paced-sender stall with a heavy child
//! resident is attributed to memory residency: the box runs ~15 GB of commit
//! over physical RAM with ~5 GB free, and when the heavy child's working set
//! grows Windows trims the OTHER working sets — SongPlayer's frame pools and the
//! NDI SDK's buffers — so the paced submit takes hard page faults (#168 measured
//! the SDK submit at 40–115 ms/frame with a resident child vs 6–17 ms without).
//! Priority class / timer resolution / TIME_CRITICAL audio protect CPU time,
//! never residency.
//!
//! The fix: at startup `process_start::apply_min_working_set` calls
//! `SetProcessWorkingSetSizeEx(GetCurrentProcess(), min, max,
//! QUOTA_LIMITS_HARDWS_MIN_ENABLE | QUOTA_LIMITS_HARDWS_MAX_DISABLE)` so the
//! memory manager NEVER trims SongPlayer below `min` (a HARD minimum), while the
//! maximum stays SOFT (SongPlayer may still grow past it when RAM allows). The
//! minimum does not pre-allocate or commit anything: it protects the pages
//! SongPlayer actually has resident, up to `min`.
//!
//! This module is pure + Linux-tested + mutation-scored: the setting parse and
//! clamp, MB→bytes, the (min, max, flags) plan and the grep-stable outcome line.
//! The Win32 calls live in `process_start.rs` (`#[cfg(windows)]`,
//! `mutants::skip`); the flag constants here are compile-time asserted equal to
//! the `windows-sys` ones there.

/// The DB setting that sizes SongPlayer's hard minimum working set, in MiB.
/// `0` disables the call; read once at startup (a change needs a restart).
pub const SETTING_KEY: &str = "sp_min_working_set_mb";

/// Default hard minimum when the setting is absent / unparseable / negative:
/// 3072 MiB.
///
/// Sized from what the playing process holds resident (the box has no direct
/// SongPlayer working-set read yet — this round's `working_set_mb` telemetry
/// adds it): per PLAYING 1440p paced output ≈ 12-frame decode look-ahead +
/// the 6-buffer frame-pool class + the 2-deep submit handoff + the pacer's
/// repeat frame + the SDK holdover ≈ 21 NV12 buffers × 5.5 MB ≈ 115 MB, plus
/// the Media Foundation decoder and the NDI SDK's per-sender compression
/// buffers (a few hundred MB per playing output); every IDLE output holds its
/// NV12 + BGRA black (≈ 20 MB); plus the audio rings, preview encoder, tokio /
/// axum / sqlite. That is ≈ 1–2 GB with two outputs playing and the rest idle;
/// 3072 MiB covers it with headroom. Over-sizing costs no RAM (the minimum
/// only guarantees pages SongPlayer actually has resident are never trimmed),
/// and the per-minute `working_set_mb` field lets the box re-size it.
pub const SP_MIN_WS_DEFAULT_MB: u32 = 3072;

/// Lower clamp for a non-zero setting: a hard minimum below 256 MiB would not
/// cover even two playing outputs' frame buffers, so it protects nothing useful.
pub const SP_MIN_WS_FLOOR_MB: u32 = 256;

/// Upper clamp: 8 GiB. A larger hard minimum reserves more of the box's
/// resident-available memory than SongPlayer can use and makes the call more
/// likely to be refused (`ERROR_NO_SYSTEM_RESOURCES`).
pub const SP_MIN_WS_CEIL_MB: u32 = 8192;

/// `QUOTA_LIMITS_HARDWS_MIN_ENABLE` (windows-sys `Win32::System::Memory`, 0x1):
/// the minimum is a HARD limit — never trimmed below.
pub const QUOTA_HARDWS_MIN_ENABLE: u32 = 0x1;

/// `QUOTA_LIMITS_HARDWS_MAX_DISABLE` (0x8): the maximum stays SOFT, so
/// SongPlayer can still grow past it when memory is available.
pub const QUOTA_HARDWS_MAX_DISABLE: u32 = 0x8;

/// The flags word passed to `SetProcessWorkingSetSizeEx`: hard min, soft max
/// (`0x9`).
pub const HARD_MIN_FLAGS: u32 = QUOTA_HARDWS_MIN_ENABLE | QUOTA_HARDWS_MAX_DISABLE;

/// Bytes per MiB.
const MIB: u64 = 1_048_576;

/// A MiB count as bytes (`mb × 1 048 576`), saturating at `usize::MAX` on a
/// narrower target. Pure.
pub fn mb_to_bytes(mb: u32) -> usize {
    usize::try_from(mb as u64 * MIB).unwrap_or(usize::MAX)
}

/// A byte count as whole MiB (rounded down). Pure.
pub fn bytes_to_mb(bytes: u64) -> u64 {
    bytes / MIB
}

/// Parse a MiB-sized memory-limit setting (shared by `sp_min_working_set_mb`
/// here and the heavy child's `heavy_max_working_set_mb`):
///
/// - `0` → `0` (disabled);
/// - a positive integer → clamped into `floor..=ceil`;
/// - absent / unparseable / negative → `default`.
///
/// Pure — the WARN for an ignored value lives in the impure caller.
pub fn parse_mb_setting(raw: Option<&str>, default: u32, floor: u32, ceil: u32) -> u32 {
    // Parsed as UNSIGNED: a negative (or > u64::MAX) value simply fails to
    // parse and takes the default — no sign guard to get wrong.
    match raw.and_then(|s| s.trim().parse::<u64>().ok()) {
        Some(0) => 0,
        Some(v) => v.clamp(floor as u64, ceil as u64) as u32,
        None => default,
    }
}

/// `true` when a PRESENT MiB setting was not used as written — unparseable,
/// negative, or outside `0 | floor..=ceil` (so the caller WARNs). An absent
/// value is not a warning (the default is the intended behaviour). Pure.
pub fn mb_setting_ignored(raw: Option<&str>, floor: u32, ceil: u32) -> bool {
    match raw {
        None => false,
        Some(r) => !r
            .trim()
            .parse::<u64>()
            .is_ok_and(|v| v == 0 || (floor as u64..=ceil as u64).contains(&v)),
    }
}

/// Parse the `sp_min_working_set_mb` setting: `0` → disabled, a positive
/// value clamped into `SP_MIN_WS_FLOOR_MB..=SP_MIN_WS_CEIL_MB`, absent /
/// unparseable / negative → [`SP_MIN_WS_DEFAULT_MB`]. Pure.
pub fn parse_min_working_set_mb(raw: Option<&str>) -> u32 {
    parse_mb_setting(
        raw,
        SP_MIN_WS_DEFAULT_MB,
        SP_MIN_WS_FLOOR_MB,
        SP_MIN_WS_CEIL_MB,
    )
}

/// [`mb_setting_ignored`] for `sp_min_working_set_mb`. Pure.
pub fn min_working_set_setting_ignored(raw: Option<&str>) -> bool {
    mb_setting_ignored(raw, SP_MIN_WS_FLOOR_MB, SP_MIN_WS_CEIL_MB)
}

/// The `SetProcessWorkingSetSizeEx` arguments for a hard minimum of `mb` MiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkingSetPlan {
    /// `dwMinimumWorkingSetSize` — the HARD floor, bytes.
    pub min_bytes: usize,
    /// `dwMaximumWorkingSetSize` — a SOFT ceiling (`2 × min`); the API requires
    /// `max ≥ min`, and with `HARDWS_MAX_DISABLE` it never caps SongPlayer.
    pub max_bytes: usize,
    /// [`HARD_MIN_FLAGS`].
    pub flags: u32,
}

/// Plan the hard-minimum call for `mb` MiB; `None` when `mb == 0` (disabled).
/// Pure.
pub fn plan_hard_min(mb: u32) -> Option<WorkingSetPlan> {
    if mb == 0 {
        return None;
    }
    let min_bytes = mb_to_bytes(mb);
    Some(WorkingSetPlan {
        min_bytes,
        max_bytes: min_bytes.saturating_mul(2),
        flags: HARD_MIN_FLAGS,
    })
}

/// Render a Win32 outcome for the log: `ok` or `failed(err=<GetLastError>)`.
fn outcome(r: Result<(), u32>) -> String {
    match r {
        Ok(()) => "ok".to_string(),
        Err(e) => format!("failed(err={e})"),
    }
}

/// The grep-stable INFO line logged once at startup (`sp working set: …`).
/// `plan == None` → the disabled line. `privilege` is the
/// `SeIncreaseWorkingSetPrivilege` enable outcome, `set` the
/// `SetProcessWorkingSetSizeEx` outcome (each `Err` carries `GetLastError`).
/// Pure, exact-string tested.
pub fn residency_line(
    plan: Option<&WorkingSetPlan>,
    privilege: Result<(), u32>,
    set: Result<(), u32>,
) -> String {
    match plan {
        None => format!("sp working set: hard_min disabled ({SETTING_KEY}=0)"),
        Some(p) => format!(
            "sp working set: hard_min_mb={} max_mb={} flags=0x{:x} privilege={} result={}",
            bytes_to_mb(p.min_bytes as u64),
            bytes_to_mb(p.max_bytes as u64),
            p.flags,
            outcome(privilege),
            outcome(set),
        ),
    }
}

#[cfg(test)]
#[path = "process_residency_tests.rs"]
mod tests;
