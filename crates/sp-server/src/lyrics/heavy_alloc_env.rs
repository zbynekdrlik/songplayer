//! #168 / #207 — the allocator environment for the heavy SEPARATION child.
//!
//! Desktop torch has no CPU caching allocator: every activation tensor is an
//! `_aligned_malloc`/`_aligned_free` on the UCRT heap, and blocks over the NT
//! heap's ~1 MiB VirtualAlloc threshold go straight to `VirtualAlloc`/
//! `VirtualFree` — each freed page is decommitted and demand-zero-faulted again
//! on the next inference step (~193k page faults/s on the box, #168 round 2b).
//! With a mimalloc override injected into the venv interpreter
//! (`bootstrap_venv_exe`), the mimalloc env turns that per-step churn into a
//! one-time cost. Two MODES ([`AllocMode`], the operator `heavy_alloc_mode`):
//!
//! - **`Retained`** (today's #168 default) — commit the reserved arena UP FRONT
//!   (`MIMALLOC_ARENA_EAGER_COMMIT=1`), never/rarely decommit
//!   (`MIMALLOC_PURGE_DELAY=<purge_delay_ms>`, `-1` = never), reserve one 4 GiB
//!   arena (`MIMALLOC_RESERVE_OS_MEMORY=4GiB`). The first-touch fault storm is
//!   paid ONCE — at the price of ~9 GB commit held for the child's whole run.
//! - **`Lazy`** (#207 phase-3) — do NOT commit the arena up front
//!   (`MIMALLOC_ARENA_EAGER_COMMIT=0`): the 4 GiB reserve stays reserved-not-
//!   committed and commit grows with first touch, returned to the OS after the
//!   purge delay. The phase-2 box measurement (issue #207 comment 5791417188)
//!   showed the purge delay is NOT the commit lever over an EAGER-committed
//!   arena (both `-1` and `10000` held 8973–8977 MB) — EAGER COMMIT is. A
//!   "never purge" delay makes no sense with lazy commit (commit would only
//!   grow), so a negative `purge_delay_ms` substitutes
//!   [`LAZY_DEFAULT_PURGE_DELAY_MS`] (10 s).
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

/// The reserved-arena size. One arena reserved (RETAINED: + committed) at start;
/// under the 10 GiB per-child job cap. Same reserve in both modes — LAZY only
/// changes whether it is committed up front.
pub const RESERVE_OS_MEMORY: &str = "4GiB";

/// `MIMALLOC_ARENA_EAGER_COMMIT` for RETAINED mode — commit the reserved arena
/// up front (today's #168 behaviour): the first-touch fault storm is paid once.
pub const RETAINED_EAGER_COMMIT: &str = "1";
/// `MIMALLOC_ARENA_EAGER_COMMIT` for LAZY mode — do NOT commit the reserved arena
/// up front; it stays reserved-not-committed and commit grows with first touch,
/// returned after the purge delay (#207 phase-2: eager commit, not the purge
/// delay, is the lever that holds the child's ~9 GB commit).
pub const LAZY_EAGER_COMMIT: &str = "0";

/// The LAZY-mode purge delay (ms) substituted when the operator setting is
/// "never purge" (`purge_delay_ms < 0`). A never-purge lazy heap would grow
/// commit without ever returning it — defeating lazy mode — so 10 s is used.
pub const LAZY_DEFAULT_PURGE_DELAY_MS: i64 = 10_000;

/// Upper bound (ms) of an explicit RETAINED `MIMALLOC_PURGE_DELAY`. Above it — or
/// below `-1` — [`emit_purge_delay`] falls back to `-1` (never purge, today's
/// default). 10 minutes is well past any decommit cadence worth measuring.
pub const PURGE_DELAY_MAX_MS: i64 = 600_000;

/// #207: how the heavy separation child's injected mimalloc heap manages OS
/// commit. The operator `heavy_alloc_mode` setting parses into this
/// ([`crate::lyrics::heavy_containment::parse_alloc_mode`]); `Retained` is the
/// default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AllocMode {
    /// Commit the arena up front, hold it (today's #168 retained heap).
    Retained,
    /// Reserve-not-commit the arena; commit grows with touch, purged after the
    /// delay (#207 phase-3).
    Lazy,
}

impl AllocMode {
    /// The lowercase token for the `alloc_mode=` field in the `heavy child
    /// contained` line and the value the operator setting carries. Read by the
    /// `heavy child contained` formatter, which is dead in the non-Windows lib
    /// target.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            AllocMode::Retained => "retained",
            AllocMode::Lazy => "lazy",
        }
    }
}

/// #207: the RETAINED `MIMALLOC_PURGE_DELAY` value string for a requested delay.
/// `-1` (never decommit — today's default) stays `-1`; a value in `0..=600_000`
/// ms emits that number; any other value (`< -1` or `> 600_000`) falls back to
/// `-1`. Pure — the out-of-range WARN lives in the settings/parse layer.
fn emit_purge_delay(purge_delay_ms: i64) -> String {
    if (0..=PURGE_DELAY_MAX_MS).contains(&purge_delay_ms) {
        purge_delay_ms.to_string()
    } else {
        "-1".to_string()
    }
}

/// #207: the LAZY `MIMALLOC_PURGE_DELAY` value string. A non-negative delay is
/// used verbatim; a negative delay (e.g. the `-1` "never purge" default, which
/// makes no sense with lazy commit) substitutes [`LAZY_DEFAULT_PURGE_DELAY_MS`]
/// (10 s). Pure.
fn lazy_purge_delay(purge_delay_ms: i64) -> String {
    if purge_delay_ms >= 0 {
        purge_delay_ms.to_string()
    } else {
        LAZY_DEFAULT_PURGE_DELAY_MS.to_string()
    }
}

/// The three mimalloc env pairs for the heavy separation child, in
/// `[purge, eager-commit, reserve]` order. Pure — no I/O. Applied verbatim next
/// to the VRAM cap from `gpu_policy::env_for_child`.
///
/// `mode` + `purge_delay_ms` are the live operator settings
/// (`heavy_alloc_mode` / `heavy_purge_delay_ms`, via
/// [`crate::lyrics::heavy_slot::current_containment`]). `Retained` keeps the
/// #168 eager-committed heap (`purge_delay_ms` `-1` = never decommit); `Lazy`
/// turns eager commit OFF so the box can return the child's ~9 GB commit, with
/// a negative delay defaulting to 10 s (a never-purge lazy heap only grows).
pub(crate) fn heavy_alloc_env(mode: AllocMode, purge_delay_ms: i64) -> Vec<(String, String)> {
    let (purge, eager) = match mode {
        AllocMode::Retained => (emit_purge_delay(purge_delay_ms), RETAINED_EAGER_COMMIT),
        AllocMode::Lazy => (lazy_purge_delay(purge_delay_ms), LAZY_EAGER_COMMIT),
    };
    vec![
        (ENV_PURGE_DELAY.to_string(), purge),
        (ENV_ARENA_EAGER_COMMIT.to_string(), eager.to_string()),
        (
            ENV_RESERVE_OS_MEMORY.to_string(),
            RESERVE_OS_MEMORY.to_string(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RETAINED at the `-1` (never-purge) default must be EXACTLY these five
    /// pairs, in order — a wrong var name or value is silently ignored by
    /// mimalloc (no effect), so the exact set is pinned. The heap trio is
    /// today's #168 output; `MIMALLOC_VERBOSE`/`MIMALLOC_SHOW_STATS` are the
    /// #207 round-3b diagnostic addition (both modes).
    #[test]
    fn retained_alloc_env_is_exactly_the_retained_heap_trio() {
        assert_eq!(
            heavy_alloc_env(AllocMode::Retained, -1),
            vec![
                ("MIMALLOC_PURGE_DELAY".to_string(), "-1".to_string()),
                ("MIMALLOC_ARENA_EAGER_COMMIT".to_string(), "1".to_string()),
                ("MIMALLOC_RESERVE_OS_MEMORY".to_string(), "4GiB".to_string()),
                ("MIMALLOC_VERBOSE".to_string(), "1".to_string()),
                ("MIMALLOC_SHOW_STATS".to_string(), "1".to_string()),
            ]
        );
    }

    #[test]
    fn heavy_alloc_env_has_exactly_five_pairs() {
        assert_eq!(heavy_alloc_env(AllocMode::Retained, -1).len(), 5);
        assert_eq!(heavy_alloc_env(AllocMode::Lazy, -1).len(), 5);
    }

    /// #207 round-3b: both modes emit `MIMALLOC_VERBOSE=1` +
    /// `MIMALLOC_SHOW_STATS=1` as the trailing pair — the separation child
    /// prints its effective options at init and its reserved/committed/peak
    /// stats at exit, both to stderr, so the box can confirm the env was
    /// actually applied without a rebuild.
    #[test]
    fn heavy_alloc_env_emits_verbose_and_show_stats_in_both_modes() {
        for mode in [AllocMode::Retained, AllocMode::Lazy] {
            let env = heavy_alloc_env(mode, -1);
            assert_eq!(
                env[3],
                ("MIMALLOC_VERBOSE".to_string(), "1".to_string()),
                "mode {mode:?}"
            );
            assert_eq!(
                env[4],
                ("MIMALLOC_SHOW_STATS".to_string(), "1".to_string()),
                "mode {mode:?}"
            );
        }
    }

    /// RETAINED keeps eager-commit=1 + reserve=4GiB regardless of the delay —
    /// only `MIMALLOC_PURGE_DELAY` varies.
    #[test]
    fn retained_keeps_the_reserve_and_eager_commit_pairs() {
        let env = heavy_alloc_env(AllocMode::Retained, 1000);
        assert_eq!(
            env[1],
            ("MIMALLOC_ARENA_EAGER_COMMIT".to_string(), "1".to_string())
        );
        assert_eq!(
            env[2],
            ("MIMALLOC_RESERVE_OS_MEMORY".to_string(), "4GiB".to_string())
        );
    }

    /// #207: RETAINED `-1` keeps the never-decommit default (the load-bearing
    /// knob — `0` would decommit on free and bring the fault storm back).
    #[test]
    fn retained_purge_delay_minus_one_is_never_decommit() {
        assert_eq!(heavy_alloc_env(AllocMode::Retained, -1)[0].1, "-1");
    }

    /// #207: RETAINED `0` emits `0` (immediate decommit — an explicit choice).
    #[test]
    fn retained_purge_delay_zero_emits_zero() {
        let env = heavy_alloc_env(AllocMode::Retained, 0);
        assert_eq!(env[0].0, "MIMALLOC_PURGE_DELAY");
        assert_eq!(env[0].1, "0");
    }

    /// #207: RETAINED finite in-range delay emits that number of ms verbatim.
    #[test]
    fn retained_purge_delay_finite_emits_the_number() {
        assert_eq!(heavy_alloc_env(AllocMode::Retained, 1000)[0].1, "1000");
        assert_eq!(heavy_alloc_env(AllocMode::Retained, 600_000)[0].1, "600000");
    }

    /// #207: RETAINED above the 600 000 ms cap falls back to `-1`.
    #[test]
    fn retained_purge_delay_above_cap_falls_back_to_never() {
        assert_eq!(heavy_alloc_env(AllocMode::Retained, 600_001)[0].1, "-1");
    }

    /// #207: RETAINED below `-1` is invalid and falls back to `-1`.
    #[test]
    fn retained_purge_delay_below_minus_one_falls_back_to_never() {
        assert_eq!(heavy_alloc_env(AllocMode::Retained, -5)[0].1, "-1");
    }

    /// #207 phase-3: LAZY at the `-1` (never-purge) default — eager commit OFF,
    /// the same 4 GiB reserve, and the 10 s LAZY default purge delay (a
    /// never-purge lazy heap only grows, so `-1` substitutes 10000), plus the
    /// #207 round-3b diagnostic pair. The whole five-pair vector is pinned.
    #[test]
    fn lazy_alloc_env_turns_eager_commit_off_and_defaults_purge_to_10s() {
        assert_eq!(
            heavy_alloc_env(AllocMode::Lazy, -1),
            vec![
                ("MIMALLOC_PURGE_DELAY".to_string(), "10000".to_string()),
                ("MIMALLOC_ARENA_EAGER_COMMIT".to_string(), "0".to_string()),
                ("MIMALLOC_RESERVE_OS_MEMORY".to_string(), "4GiB".to_string()),
                ("MIMALLOC_VERBOSE".to_string(), "1".to_string()),
                ("MIMALLOC_SHOW_STATS".to_string(), "1".to_string()),
            ]
        );
    }

    /// #207 phase-3: LAZY with a non-negative delay uses it verbatim (eager
    /// still OFF, reserve still 4 GiB).
    #[test]
    fn lazy_alloc_env_uses_a_positive_delay_verbatim() {
        let env = heavy_alloc_env(AllocMode::Lazy, 2500);
        assert_eq!(
            env[0],
            ("MIMALLOC_PURGE_DELAY".to_string(), "2500".to_string())
        );
        assert_eq!(
            env[1],
            ("MIMALLOC_ARENA_EAGER_COMMIT".to_string(), "0".to_string())
        );
        assert_eq!(
            env[2],
            ("MIMALLOC_RESERVE_OS_MEMORY".to_string(), "4GiB".to_string())
        );
    }

    /// #207 phase-3: LAZY at exactly `0` emits `0` (immediate purge) — the
    /// `>= 0` boundary (a `> 0` mutant would substitute 10000 here).
    #[test]
    fn lazy_alloc_env_delay_zero_emits_zero() {
        assert_eq!(heavy_alloc_env(AllocMode::Lazy, 0)[0].1, "0");
    }

    /// #207: `alloc_mode` renders the lowercase operator token.
    #[test]
    fn alloc_mode_as_str_is_the_lowercase_token() {
        assert_eq!(AllocMode::Retained.as_str(), "retained");
        assert_eq!(AllocMode::Lazy.as_str(), "lazy");
    }
}
