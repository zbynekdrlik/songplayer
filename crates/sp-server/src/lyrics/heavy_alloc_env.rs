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

/// The three env pairs that make the injected mimalloc heap RETAIN memory for
/// the heavy separation child. Pure — no I/O. Applied verbatim next to the VRAM
/// cap from `gpu_policy::env_for_child`.
pub fn heavy_alloc_env() -> Vec<(String, String)> {
    vec![
        (ENV_PURGE_DELAY.to_string(), "-1".to_string()),
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

    /// The retained-heap env must be EXACTLY these three pairs, in order — a
    /// wrong var name or value is silently ignored by mimalloc (no effect), so
    /// the exact set is pinned.
    #[test]
    fn heavy_alloc_env_is_exactly_the_retained_heap_trio() {
        assert_eq!(
            heavy_alloc_env(),
            vec![
                ("MIMALLOC_PURGE_DELAY".to_string(), "-1".to_string()),
                ("MIMALLOC_ARENA_EAGER_COMMIT".to_string(), "1".to_string()),
                ("MIMALLOC_RESERVE_OS_MEMORY".to_string(), "4GiB".to_string()),
            ]
        );
    }

    #[test]
    fn heavy_alloc_env_has_exactly_three_pairs() {
        assert_eq!(heavy_alloc_env().len(), 3);
    }

    /// Never-purge is the load-bearing knob: `0` would decommit on free and
    /// bring the fault storm back.
    #[test]
    fn purge_delay_is_never() {
        let env = heavy_alloc_env();
        let (k, v) = &env[0];
        assert_eq!(k, "MIMALLOC_PURGE_DELAY");
        assert_eq!(v, "-1");
    }
}
