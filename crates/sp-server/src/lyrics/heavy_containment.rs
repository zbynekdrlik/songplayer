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
    /// default is the TOP 4 logical cores (#168 round 5 — the measured
    /// grid-holding block; the wall keeps the rest).
    pub(crate) affinity_mask: u64,
    /// Whether the child's process memory priority is lowered to
    /// `MEMORY_PRIORITY_LOW`. Always `true` today; kept as a field for the
    /// applied-containment log line. Read only by the `#[cfg(windows)]` Job
    /// Object seam, so it is dead in the non-Windows lib target.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) memory_priority_low: bool,
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
/// the TOP 4 logical cores, derived from the count — never a literal.
/// `logical_cores` is clamped to `1..=64` (a Windows affinity mask is one
/// processor group, ≤ 64 bits; a reported `0` is treated as `1`); a box with
/// fewer than 4 logical cores simply gets all of them.
///
/// #168 round 5 — was the UPPER HALF of the cores (24 → `fff000`). Round 4's
/// paced measurement (issue #168 comment 5779505749) isolated the stall channel
/// as core PLACEMENT: the SAME separation child stalls the NDI `send_video_async`
/// call to 30–105 ms/min while it may run on the upper 12 logical cores (the old
/// `fff000` default — W1: 0/12 minutes ≤ 20 ms) and holds the SP-slow grid at
/// 3.8–19.3 ms in 10/10 minutes when confined to 4 logical cores (`f00000` —
/// W3b). The NDI SDK's unpinned compress/send threads (HIGH class) otherwise
/// land on a physical core whose SMT sibling runs an AVX-saturating RoFormer
/// thread; a 2-physical-core (4-logical) block leaves 10 of 12 physical cores
/// free of the child, while a 1-physical-core block (`c00000` — W3) starves it.
/// An explicit `heavy_cpu_affinity_mask` setting still overrides this default
/// (see [`parse_affinity_mask`]).
///
/// E.g. 24 cores → `0xF00000` (cores 20–23), 8 cores → `0xF0` (cores 4–7),
/// 6 → `0x3C`, 4 → `0xF`, 2 → `0x3`. Pure.
pub(crate) fn default_affinity_mask(logical_cores: usize) -> u64 {
    let cores = logical_cores.clamp(1, 64);
    // The child gets the TOP `top` logical cores; a box with < 4 gets all of
    // them. `top <= cores`, so `shift = cores - top` never underflows and the
    // mask's highest set bit is `cores - 1` (≤ 63) — no shift overflow.
    let top = cores.min(6); // RED: GREEN sets the 4-core block via `.min(4)`
    let shift = cores - top; // the lower cores reserved for the wall/OBS/Resolume
    ((1u64 << top) - 1) << shift
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

/// Resolve the live [`Containment`] from the two operator settings + the box's
/// logical-core count. The impure caller ([`crate::lyrics::heavy_slot`]) reads
/// the settings + core count and calls this pure fn. Pure.
pub(crate) fn containment_from_settings(
    cap_str: Option<&str>,
    mask_str: Option<&str>,
    logical_cores: usize,
) -> Containment {
    Containment {
        cpu_cap_pct: clamp_cap_pct(cap_str),
        affinity_mask: parse_affinity_mask(mask_str, logical_cores),
        memory_priority_low: true,
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
