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
    /// default is the UPPER half of the cores (the wall keeps the lower half).
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
/// the UPPER half of the cores (the wall processes keep the lower half), derived
/// from the count — never a literal. `logical_cores` is clamped to `1..=64` (a
/// Windows affinity mask is one processor group, ≤ 64 bits; a reported `0` is
/// treated as `1`). E.g. 8 cores → `0xF0` (cores 4–7), 24 cores → `0xFFF000`
/// (cores 12–23). Pure.
pub(crate) fn default_affinity_mask(logical_cores: usize) -> u64 {
    let cores = logical_cores.clamp(1, 64);
    let lower_half = cores / 2; // cores reserved for the wall processes
    let upper_count = cores - lower_half; // heavy children get the upper half (the extra core when odd)
    // `upper_count` is 1..=32 for `cores` 1..=64, so `1u64 << upper_count` never
    // overflows and no all-bits special case is reachable.
    ((1u64 << upper_count) - 1) << lower_half
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
/// to no cores). A well-formed non-zero override is honoured verbatim. Pure.
fn parse_affinity_mask(raw: Option<&str>, logical_cores: usize) -> u64 {
    let default = default_affinity_mask(logical_cores);
    match raw {
        Some(s) => {
            let t = s.trim();
            let hex = t
                .strip_prefix("0x")
                .or_else(|| t.strip_prefix("0X"))
                .unwrap_or(t);
            match u64::from_str_radix(hex, 16) {
                Ok(m) if m != 0 => m,
                _ => default,
            }
        }
        None => default,
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
