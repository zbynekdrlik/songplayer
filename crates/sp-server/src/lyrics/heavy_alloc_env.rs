//! #168 — the retained-heap environment for the heavy SEPARATION child.
//!
//! Desktop torch has no CPU caching allocator: every activation tensor is an
//! `_aligned_malloc`/`_aligned_free` on the UCRT heap, and blocks over the NT
//! heap's ~1 MiB VirtualAlloc threshold go straight to `VirtualAlloc`/
//! `VirtualFree` — each freed page is decommitted and demand-zero-faulted again
//! on the next inference step (~193k page faults/s on the box, #168 round 2b).
//! With a mimalloc override injected into the venv interpreter
//! (`bootstrap_venv_exe`), these three options turn that per-step churn into a
//! one-time cost:
//!
//! - `MIMALLOC_PURGE_DELAY=-1` — never decommit freed memory back to the OS
//!   (so a freed arena page is reused, not re-faulted).
//! - `MIMALLOC_ARENA_EAGER_COMMIT=1` — commit an arena's pages up front.
//! - `MIMALLOC_RESERVE_OS_MEMORY=4GiB` — reserve + commit one arena at start, so
//!   the first-touch fault storm is paid ONCE, not once per step. Fits under the
//!   10 GiB per-child Job Object cap (`heavy_slot::CHILD_JOB_MEMORY_LIMIT_BYTES`).
//!
//! Numerically invisible to the model; applied to the separation child only
//! (next to `gpu_policy::env_for_child` in `stems/separator.rs`) — the dub child
//! is light and the mtl aligner venv is untouched.

/// Env var: mimalloc purge delay (ms). `-1` = never purge/decommit.
pub const ENV_PURGE_DELAY: &str = "MIMALLOC_PURGE_DELAY";
/// Env var: eagerly commit arena pages.
pub const ENV_ARENA_EAGER_COMMIT: &str = "MIMALLOC_ARENA_EAGER_COMMIT";
/// Env var: reserve N of OS memory (one arena) up front.
pub const ENV_RESERVE_OS_MEMORY: &str = "MIMALLOC_RESERVE_OS_MEMORY";

/// The reserved-arena size. One arena reserved + committed at start pays the
/// first-touch fault cost once; under the 10 GiB per-child job cap.
pub const RESERVE_OS_MEMORY: &str = "4GiB";

/// Upper bound (ms) of an explicit `MIMALLOC_PURGE_DELAY`. Above it — or below
/// `-1` — [`emit_purge_delay`] falls back to `-1` (never purge, today's default).
/// 10 minutes is well past any decommit cadence worth measuring on the box.
pub const PURGE_DELAY_MAX_MS: i64 = 600_000;

/// #207: the `MIMALLOC_PURGE_DELAY` value string for a requested delay. `-1`
/// (never decommit — today's default) stays `-1`; a value in `0..=600_000` ms
/// emits that number; any other value (`< -1` or `> 600_000`) falls back to `-1`.
/// Pure — the out-of-range WARN lives in the settings/parse layer, never here.
fn emit_purge_delay(purge_delay_ms: i64) -> String {
    if (0..=PURGE_DELAY_MAX_MS).contains(&purge_delay_ms) {
        purge_delay_ms.to_string()
    } else {
        "-1".to_string()
    }
}

/// The three env pairs that make the injected mimalloc heap RETAIN memory for
/// the heavy separation child. Pure — no I/O. Applied verbatim next to the VRAM
/// cap from `gpu_policy::env_for_child`.
///
/// #207: `purge_delay_ms` is the operator `heavy_purge_delay_ms` setting
/// (via [`crate::lyrics::heavy_slot::current_containment`]): `-1` keeps the
/// retained-heap default, `0..=600_000` sets a finite decommit delay so the box
/// can measure returning the child's ~9 GB commit without the fault storm.
pub fn heavy_alloc_env(purge_delay_ms: i64) -> Vec<(String, String)> {
    vec![
        (
            ENV_PURGE_DELAY.to_string(),
            emit_purge_delay(purge_delay_ms),
        ),
        (ENV_ARENA_EAGER_COMMIT.to_string(), "1".to_string()),
        (
            ENV_RESERVE_OS_MEMORY.to_string(),
            RESERVE_OS_MEMORY.to_string(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The retained-heap env at the `-1` (never-purge) default must be EXACTLY
    /// these three pairs, in order — a wrong var name or value is silently
    /// ignored by mimalloc (no effect), so the exact set is pinned.
    #[test]
    fn heavy_alloc_env_is_exactly_the_retained_heap_trio() {
        assert_eq!(
            heavy_alloc_env(-1),
            vec![
                ("MIMALLOC_PURGE_DELAY".to_string(), "-1".to_string()),
                ("MIMALLOC_ARENA_EAGER_COMMIT".to_string(), "1".to_string()),
                ("MIMALLOC_RESERVE_OS_MEMORY".to_string(), "4GiB".to_string()),
            ]
        );
    }

    #[test]
    fn heavy_alloc_env_has_exactly_three_pairs() {
        assert_eq!(heavy_alloc_env(-1).len(), 3);
    }

    /// The two retained-heap knobs (eager-commit + reserve) never change with
    /// the purge delay — only `MIMALLOC_PURGE_DELAY` varies.
    #[test]
    fn heavy_alloc_env_keeps_the_reserve_and_eager_commit_pairs() {
        let env = heavy_alloc_env(1000);
        assert_eq!(
            env[1],
            ("MIMALLOC_ARENA_EAGER_COMMIT".to_string(), "1".to_string())
        );
        assert_eq!(
            env[2],
            ("MIMALLOC_RESERVE_OS_MEMORY".to_string(), "4GiB".to_string())
        );
    }

    /// #207: `-1` keeps the never-decommit default (the load-bearing knob — `0`
    /// would decommit on free and bring the fault storm back).
    #[test]
    fn purge_delay_minus_one_is_never_decommit() {
        assert_eq!(heavy_alloc_env(-1)[0].1, "-1");
    }

    /// #207: `0` emits `0` (immediate decommit — an explicit operator choice).
    #[test]
    fn purge_delay_zero_emits_zero() {
        let env = heavy_alloc_env(0);
        assert_eq!(env[0].0, "MIMALLOC_PURGE_DELAY");
        assert_eq!(env[0].1, "0");
    }

    /// #207: a finite in-range delay emits that number of ms verbatim.
    #[test]
    fn purge_delay_finite_emits_the_number() {
        assert_eq!(heavy_alloc_env(1000)[0].1, "1000");
        assert_eq!(heavy_alloc_env(600_000)[0].1, "600000");
    }

    /// #207: above the 600 000 ms cap falls back to `-1` (never decommit).
    #[test]
    fn purge_delay_above_cap_falls_back_to_never() {
        assert_eq!(heavy_alloc_env(600_001)[0].1, "-1");
    }

    /// #207: a value below `-1` is invalid and falls back to `-1`.
    #[test]
    fn purge_delay_below_minus_one_falls_back_to_never() {
        assert_eq!(heavy_alloc_env(-5)[0].1, "-1");
    }
}
