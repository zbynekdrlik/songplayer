//! #203 — pure decision core for OS-level containment of the heavy background
//! children (stems separation / lyrics vocal-isolation / mtl align / dub).
//!
//! Box test 7 (#168, 21.9.2026) measured the NDI SDK `send_video_async` at
//! 40–115 ms per 1440p frame with a heavy child resident (grid slot 33 ms) →
//! genlock pacing collapse (88–90 % late), and the same contention produced the
//! Sunday audio holes (#192). The child already runs `BELOW_NORMAL` with a
//! 3-thread cap (#162), but a priority class only ORDERS runnable threads — with
//! free cores it still runs at full speed and saturates the memory bus the SDK's
//! compression threads, OBS and Resolume all need. This module turns the two
//! operator settings (`heavy_cpu_cap_pct`, `heavy_cpu_affinity_mask`) plus the
//! box's logical-core count into a [`Containment`] that the Windows Job Object
//! seam ([`crate::lyrics::heavy_slot`]) applies to every heavy child.
//!
//! Pure + unit-tested + mutation-clean; the Win32 calls live in `heavy_slot.rs`
//! (cfg(windows), `mutants::skip`).

use crate::lyrics::heavy_alloc_env::AllocMode;

/// The CPU hard-cap percentage is clamped into this inclusive range; an
/// absent/unparseable setting falls back to [`CPU_CAP_DEFAULT_PCT`].
pub(crate) const CPU_CAP_MIN_PCT: u8 = 5;
/// Upper clamp bound for the CPU hard-cap percentage (100 % = no cap, today's
/// behaviour).
pub(crate) const CPU_CAP_MAX_PCT: u8 = 100;
/// Default CPU hard-cap when the setting is absent/unparseable: 25 % of TOTAL
/// machine CPU time, chosen for the wall (SongPlayer/OBS/Resolume always win).
pub(crate) const CPU_CAP_DEFAULT_PCT: u8 = 25;

/// The applied OS-level containment for one heavy child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Containment {
    /// Job Object CPU hard-cap, percent of TOTAL machine CPU time (`5..=100`).
    pub(crate) cpu_cap_pct: u8,
    /// Job Object affinity mask — which logical cores the child may run on. The
    /// default is the TOP 3 logical cores (#168 round 8 — the measured best
    /// resident-child block; the wall keeps the rest).
    pub(crate) affinity_mask: u64,
    /// Whether the child's process memory priority is lowered to
    /// `MEMORY_PRIORITY_LOW`. Always `true` today; kept as a field for the
    /// applied-containment log line. Read only by the `#[cfg(windows)]` Job
    /// Object seam, so it is dead in the non-Windows lib target.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) memory_priority_low: bool,
    /// #207: the `MIMALLOC_PURGE_DELAY` (ms) the separation child's allocator env
    /// carries — `-1` = never decommit (the #168 retained-heap default),
    /// `0..=600_000` = a finite decommit delay so the box can measure returning
    /// the child's ~9 GB commit. Read cross-platform by
    /// [`crate::lyrics::heavy_slot::current_containment`] at separation spawn.
    pub(crate) purge_delay_ms: i64,
    /// #207 phase-3: the separation child's mimalloc commit mode
    /// (`heavy_alloc_mode`): `Retained` (today's eager-committed heap) or `Lazy`
    /// (reserve-not-commit, so the box can return the child's ~9 GB commit —
    /// eager commit, not the purge delay, is the lever, comment 5791417188).
    /// Read cross-platform by `stems/separator.rs` at separation spawn.
    pub(crate) alloc_mode: AllocMode,
    /// #207 round-3c: the `MIMALLOC_RESERVE_OS_MEMORY` arena size in GiB
    /// (`heavy_alloc_reserve_gib`, `1..=8`, default 4) — round-3b's mimalloc
    /// self-report showed the eager-committed 4 GiB arena IS the ~4 GiB piece
    /// of the child's 8.7 GiB peak commit (`commits: 0`), so a smaller reserve
    /// is the lever ROZHODNUTÉ 3c measures. Read cross-platform by
    /// `stems/separator.rs` at separation spawn.
    pub(crate) reserve_gib: u8,
}

/// The Job Object `CpuRate` unit for a cap percentage: hundredths of a percent,
/// so 100 % CPU maps to 10000 and 25 % to 2500. `pct * 100`. Pure. Consumed only
/// by the `#[cfg(windows)]` Job Object seam, so it is dead in the non-Windows lib
/// target (still exercised by the unit tests).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn cpu_rate_from_pct(pct: u8) -> u32 {
    pct as u32 * 100
}

/// The DEFAULT affinity mask for a box with `logical_cores` logical processors:
/// the TOP 3 logical cores, derived from the count — never a literal.
/// `logical_cores` is clamped to `1..=64` (a Windows affinity mask is one
/// processor group, ≤ 64 bits; a reported `0` is treated as `1`); a box with
/// fewer than 3 logical cores simply gets all of them.
///
/// #168 round 8 — was the TOP 4 logical cores (round 5, 24 → `f00000`). Round
/// 7's paced measurement (issue #147 comment 5786765465, 22./23.9.2026) held
/// one variable per 15-minute window with the live wall on program and found
/// the receiver's residual `dropped_due` scales with the resident separation
/// child's CPU intensity, not its phase or memory pressure:
///
/// - **`f00000` (top 4 logical cores, 4 threads ≈ 1.5–2.0 cores)** — receiver
///   `dropped_due` 0.9–1.35/min, sender `submit_call_us_max` ≤ 20 ms in only
///   10–15 of 16 minutes (W1, W4).
/// - **`e00000` (top 3 logical cores, 3 threads ≈ 1.0–1.1 core)** — receiver
///   `dropped_due` 0.27–0.5/min, sender ≤ 20 ms in **16/16** minutes (W5:
///   3.2–13.2 ms), measured twice (W3, W5) with no observed wall-time slowdown
///   on the separations.
/// - **`c00000` (2 logical = 1 physical)** — STARVES the child under
///   `BELOW_NORMAL` (round 4 W3: 80 CPU-s in 12 min), so 3 logical cores is the
///   floor.
///
/// The receiver reaches contract-§8 zero only with NO child resident; the
/// 3-core block is the best a resident child can have. Four AVX RoFormer threads
/// over 2 full physical cores (both SMT siblings busy) run ≈ 1.5–2.0 cores and
/// press the shared L3/DRAM the NDI SDK's compress threads need; three threads
/// on one SMT pair + one half pair run ≈ 1.0–1.1 core and roughly halve-to-
/// quarter the residual. An explicit `heavy_cpu_affinity_mask=f00000` setting
/// still overrides this default (see [`parse_affinity_mask`]) if a full-video
/// separation ever slows > 1.5×.
///
/// E.g. 24 cores → `0xE00000` (cores 21–23), 8 cores → `0xE0` (cores 5–7),
/// 6 → `0x38`, 4 → `0xE`, 3 → `0x7`, 2 → `0x3`. Pure.
pub(crate) fn default_affinity_mask(logical_cores: usize) -> u64 {
    let cores = logical_cores.clamp(1, 64);
    // The child gets the TOP `top` logical cores; a box with < 3 gets all of
    // them. `top <= cores`, so `shift = cores - top` never underflows and the
    // mask's highest set bit is `cores - 1` (≤ 63) — no shift overflow.
    let top = cores.min(3); // the top 3 logical cores — the measured best resident-child block
    let shift = cores - top; // the lower cores reserved for the wall/OBS/Resolume
    ((1u64 << top) - 1) << shift
}

/// #207 default when `heavy_purge_delay_ms` is absent / unparseable / out of the
/// valid range: `-1` (never decommit — the #168 retained-heap behaviour).
pub(crate) const PURGE_DELAY_DEFAULT_MS: i64 = -1;

/// Parse the `heavy_purge_delay_ms` setting into a mimalloc purge delay (ms).
/// Valid values are `-1` (never decommit) or `0..=600_000`; anything missing,
/// unparseable, or out of range falls back to [`PURGE_DELAY_DEFAULT_MS`]. Pure —
/// the WARN on an out-of-range value lives in the impure caller
/// (`heavy_slot::refresh_containment`), never here.
fn parse_purge_delay_ms(raw: Option<&str>) -> i64 {
    // `-1` needs no guard of its own: it IS the default, so it falls through
    // the `_` arm (a `v == -1` guard was an equivalent mutant — `delete -`
    // survived the diff-scoped mutation gate, run 35828975563).
    match raw.and_then(|s| s.trim().parse::<i64>().ok()) {
        Some(v) if (0..=crate::lyrics::heavy_alloc_env::PURGE_DELAY_MAX_MS).contains(&v) => v,
        _ => PURGE_DELAY_DEFAULT_MS,
    }
}

/// #207 phase-3: parse the `heavy_alloc_mode` setting into an [`AllocMode`].
/// A trimmed `"lazy"` selects [`AllocMode::Lazy`]; anything else — absent,
/// `"retained"`, or unrecognised — is [`AllocMode::Retained`] (the default,
/// today's eager-committed heap). Pure — the impure caller may WARN on an
/// unrecognised non-empty value.
fn parse_alloc_mode(raw: Option<&str>) -> AllocMode {
    match raw.map(str::trim) {
        Some("lazy") => AllocMode::Lazy,
        _ => AllocMode::Retained,
    }
}

/// #207 round-3c: parse the `heavy_alloc_reserve_gib` setting into the
/// `MIMALLOC_RESERVE_OS_MEMORY` arena size, GiB. Valid values are `1..=8`;
/// anything missing, unparseable, or out of range falls back to
/// [`crate::lyrics::heavy_alloc_env::RESERVE_GIB_DEFAULT`] (4). Pure — the WARN
/// on an out-of-range value lives in the impure caller
/// (`heavy_slot::refresh_containment`), never here.
fn parse_reserve_gib(raw: Option<&str>) -> u8 {
    match raw.and_then(|s| s.trim().parse::<i64>().ok()) {
        Some(v) if (1..=8).contains(&v) => v as u8,
        _ => crate::lyrics::heavy_alloc_env::RESERVE_GIB_DEFAULT,
    }
}

/// Parse + clamp the `heavy_cpu_cap_pct` setting into `5..=100`. An
/// absent/unparseable value falls back to [`CPU_CAP_DEFAULT_PCT`]; a valid
/// integer below 5 clamps up to 5, above 100 clamps down to 100. Pure.
fn clamp_cap_pct(raw: Option<&str>) -> u8 {
    match raw.and_then(|s| s.trim().parse::<i64>().ok()) {
        Some(v) => v.clamp(CPU_CAP_MIN_PCT as i64, CPU_CAP_MAX_PCT as i64) as u8,
        None => CPU_CAP_DEFAULT_PCT,
    }
}

/// Parse the `heavy_cpu_affinity_mask` setting (a hex string, optional `0x`/`0X`
/// prefix) into a core mask. An absent, unparseable, or ZERO value falls back to
/// [`default_affinity_mask`] for `logical_cores` (a zero mask would pin the child
/// to no cores). A well-formed override is CLAMPED to the cores that exist
/// ([`existing_cores_mask`]) — a bit beyond the processor count would make the
/// single extended-limit `SetInformationJobObject` call fail and drop the
/// memory ceiling with it; an override with no valid bit falls back too. Pure.
fn parse_affinity_mask(raw: Option<&str>, logical_cores: usize) -> u64 {
    let default = default_affinity_mask(logical_cores);
    let valid = existing_cores_mask(logical_cores);
    match raw {
        Some(s) => {
            let t = s.trim();
            let hex = t
                .strip_prefix("0x")
                .or_else(|| t.strip_prefix("0X"))
                .unwrap_or(t);
            match u64::from_str_radix(hex, 16) {
                Ok(m) if m & valid != 0 => m & valid,
                _ => default,
            }
        }
        None => default,
    }
}

/// The mask of ALL cores that exist on a box with `logical_cores` logical
/// processors (≥ 64 → every bit; 0 → treated as 1). Pure.
pub(crate) fn existing_cores_mask(logical_cores: usize) -> u64 {
    match logical_cores.max(1) {
        n if n >= 64 => u64::MAX,
        n => (1u64 << n) - 1,
    }
}

/// Resolve the live [`Containment`] from the five operator settings
/// (`heavy_cpu_cap_pct`, `heavy_cpu_affinity_mask`, `heavy_purge_delay_ms`,
/// `heavy_alloc_mode`, `heavy_alloc_reserve_gib` (#207 round-3c)) + the box's
/// logical-core count. The impure caller ([`crate::lyrics::heavy_slot`]) reads
/// the settings + core count and calls this pure fn. Pure.
pub(crate) fn containment_from_settings(
    cap_str: Option<&str>,
    mask_str: Option<&str>,
    purge_str: Option<&str>,
    alloc_str: Option<&str>,
    reserve_str: Option<&str>,
    logical_cores: usize,
) -> Containment {
    Containment {
        cpu_cap_pct: clamp_cap_pct(cap_str),
        affinity_mask: parse_affinity_mask(mask_str, logical_cores),
        memory_priority_low: true,
        purge_delay_ms: parse_purge_delay_ms(purge_str),
        alloc_mode: parse_alloc_mode(alloc_str),
        reserve_gib: parse_reserve_gib(reserve_str),
    }
}

/// Render an affinity mask as the lowercase hex the setting uses (no `0x`
/// prefix), for the `/api/v1/status.heavy_containment.affinity_mask` field. Pure.
pub(crate) fn affinity_mask_hex(mask: u64) -> String {
    format!("{mask:x}")
}

#[cfg(test)]
#[path = "heavy_containment_tests.rs"]
mod tests;
