//! Genlock lock-state vocabulary (#149, Lane 1, contract §7 A7.3).
//!
//! The three-state lock enum + its pure derivation, shared (WASM-safe) between
//! the `/api/v1/ndi/health` snapshot, the per-minute structured log line, and
//! (#150) the WASM dashboard badge — one definition of the LOCKED / DEGRADED /
//! UNLOCKED vocabulary the OBS indicator also speaks (camera-box#1298).
//!
//! The window counts fed to [`derive`] are produced engine-side by the
//! sp-server `playback::lock_state::EventWindow` (a 60 s ring of cumulative
//! pacing counters); this module owns only the state machine, so its rules are
//! testable with plain integers and no clock.

use serde::{Deserialize, Serialize};

/// The genlock lock state exposed on the health snapshot / log / UI badge.
/// Serialised as the exact camera-box#1298 tokens.
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone, Copy)]
pub enum LockState {
    #[serde(rename = "LOCKED")]
    Locked,
    #[serde(rename = "DEGRADED")]
    Degraded,
    #[serde(rename = "UNLOCKED")]
    Unlocked,
}

impl LockState {
    /// The wire/log token for this state (`"LOCKED" | "DEGRADED" | "UNLOCKED"`),
    /// matching the serde rename so the log line and the JSON agree.
    pub fn as_str(&self) -> &'static str {
        match self {
            LockState::Locked => "LOCKED",
            LockState::Degraded => "DEGRADED",
            LockState::Unlocked => "UNLOCKED",
        }
    }
}

/// Derive the lock state + a static reason string from the clock/pacing/receiver
/// inputs and the 60 s window event counts (contract §7 A7.3). The checks are
/// evaluated in strict precedence order — the FIRST that matches wins:
///
/// 1. `!clock_ok`               → `(Unlocked, "clock not ok")`
/// 2. `!pacing_enabled`         → `(Unlocked, "pacing disabled")`
/// 3. `connections == 0`        → `(Degraded, "no receiver")`
/// 4. any window count `> 0`    → `(Degraded, "late/repeats/resyncs in 60 s")`
/// 5. otherwise                 → `(Locked,   "locked")`
///
/// So `clock not ok` beats `pacing disabled` beats `no receiver` beats the
/// window-event check. Today (flag OFF) every output resolves to
/// `(Unlocked, "pacing disabled")`, which is correct by contract.
pub fn derive(
    clock_ok: bool,
    pacing_enabled: bool,
    connections: u32,
    late_in_window: u64,
    repeats_in_window: u64,
    resyncs_in_window: u64,
) -> (LockState, &'static str) {
    if !clock_ok {
        return (LockState::Unlocked, "clock not ok");
    }
    if !pacing_enabled {
        return (LockState::Unlocked, "pacing disabled");
    }
    if connections == 0 {
        return (LockState::Degraded, "no receiver");
    }
    if late_in_window > 0 || repeats_in_window > 0 || resyncs_in_window > 0 {
        return (LockState::Degraded, "late/repeats/resyncs in 60 s");
    }
    (LockState::Locked, "locked")
}

/// One output's lock state as fed to [`summarize`] (#150): its NDI name, the
/// derived [`LockState`], whether the output is LIVE on the wall
/// (`state == Playing`), and whether the box clock is ok. WASM-safe and pure.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputLock {
    pub name: String,
    pub state: LockState,
    pub live: bool,
    pub clock_ok: bool,
}

/// The reduced whole-box genlock lock, rendered as the dashboard's global
/// badge (#150): the summarised [`LockState`], the worst output's name (when
/// the box is not fully LOCKED), and how many outputs are live.
#[derive(Clone, Debug, PartialEq)]
pub struct LockSummary {
    pub state: LockState,
    pub worst: Option<String>,
    pub live_count: usize,
}

/// Rank a state for "which output is worst" — UNLOCKED beats DEGRADED beats
/// LOCKED (higher = worse).
fn severity(state: LockState) -> u8 {
    match state {
        LockState::Locked => 0,
        LockState::Degraded => 1,
        LockState::Unlocked => 2,
    }
}

/// The output's effective state for the summary: a LOCKED output whose clock
/// is not ok is demoted to UNLOCKED. Defensive — the engine never emits LOCKED
/// with `clock_ok == false` (see [`derive`], which returns UNLOCKED first) —
/// but the summary must never report LOCKED off a clock-broken output.
fn effective_state(o: &OutputLock) -> LockState {
    if o.state == LockState::Locked && !o.clock_ok {
        LockState::Unlocked
    } else {
        o.state
    }
}

/// Reduce the per-output lock states into ONE whole-box summary for the
/// dashboard header badge (#150), sharing the LOCKED / DEGRADED / UNLOCKED
/// vocabulary of [`LockState`] and [`derive`].
///
/// Rules (contract §7, camera-box#1298):
/// - **LOCKED** iff every LIVE output is LOCKED and the box clock is ok
///   (`worst = None`).
/// - Otherwise the **worst live state** (UNLOCKED > DEGRADED > LOCKED),
///   naming the worst output in `worst`.
/// - **No live output** → the state comes from the clock only: LOCKED (with
///   `worst = None`, rendered "no live output") when every output reports
///   `clock_ok`, else UNLOCKED. An empty slice is treated as clock-ok
///   (vacuously LOCKED / no live output).
pub fn summarize(outputs: &[OutputLock]) -> LockSummary {
    let live_count = outputs.iter().filter(|o| o.live).count();

    if live_count == 0 {
        let clock_ok = outputs.iter().all(|o| o.clock_ok);
        return LockSummary {
            state: if clock_ok {
                LockState::Locked
            } else {
                LockState::Unlocked
            },
            worst: None,
            live_count: 0,
        };
    }

    // The worst live output by effective severity. `max_by_key` keeps the last
    // maximum on ties, which is fine — any worst-tied output is a valid name.
    let worst = outputs
        .iter()
        .filter(|o| o.live)
        .max_by_key(|o| severity(effective_state(o)))
        .expect("live_count > 0 guarantees a live output");

    let worst_state = effective_state(worst);
    if worst_state == LockState::Locked {
        LockSummary {
            state: LockState::Locked,
            worst: None,
            live_count,
        }
    } else {
        LockSummary {
            state: worst_state,
            worst: Some(worst.name.clone()),
            live_count,
        }
    }
}

// ── #176: always-visible whole-box GLOBAL genlock indicator, incl. OFF ────────

/// The dashboard's always-visible global genlock state (#176). Adds a fourth
/// state `Off` to the LOCKED / DEGRADED / UNLOCKED lock vocabulary: `Off` means
/// NO output has boundary pacing enabled (`genlock_pacing=false`, the production
/// default #147) — the box free-runs on the NDI SDK clock, a deliberate
/// configuration, not a fault. Rendered grey; the other three keep their
/// green / amber / red colours (camera-box#1298). This is the #176 revision of
/// #164's "hide the badge entirely while pacing is off".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlobalLock {
    Off,
    Locked,
    Degraded,
    Unlocked,
}

/// One output's inputs to [`global_lock_summary`] (#176): its pacing-enabled
/// flag (`Off` is derived from this across ALL outputs), whether it is LIVE on
/// the wall, its derived [`LockState`], its box-clock-ok flag, and its reason
/// string. WASM-safe and pure — the sp-ui `GlobalLockBadge` builds these from
/// the `/api/v1/ndi/health` snapshot and renders the returned state (sp-ui has
/// no unit-test job, so the logic + its tests live here).
#[derive(Clone, Debug, PartialEq)]
pub struct GlobalLockInput {
    pub pacing_enabled: bool,
    pub live: bool,
    pub state: LockState,
    pub clock_ok: bool,
    pub reason: String,
}

/// The whole-box global genlock summary (#176): the [`GlobalLock`] state plus a
/// short reason string for the badge tooltip.
#[derive(Clone, Debug, PartialEq)]
pub struct GlobalLockSummary {
    pub state: GlobalLock,
    pub reason: String,
}

/// This output's effective [`LockState`] for the global summary: a LOCKED output
/// whose box clock is not ok is demoted to UNLOCKED (mirrors [`summarize`] /
/// [`effective_state`]).
fn effective_global(o: &GlobalLockInput) -> LockState {
    if o.state == LockState::Locked && !o.clock_ok {
        LockState::Unlocked
    } else {
        o.state
    }
}

/// The reason shown in the `● GENLOCK OFF` tooltip.
///
/// TIER-0 RED marker: the RED commit shipped `"pacing off"` here so
/// `off_when_no_output_has_pacing_enabled` failed on the reason assert while the
/// whole function still compiled and used every field; GREEN sets the correct
/// Slovak string.
const OFF_REASON: &str = "pacing off";

/// Reduce the per-output genlock inputs into ONE whole-box GLOBAL state for the
/// always-visible dashboard badge (#176):
///
/// - **No output has pacing enabled** → [`GlobalLock::Off`] (reason
///   `"pacing vypnuté"`) — the production default; grey, never hidden.
/// - **Pacing-enabled LIVE outputs** → the worst effective state over them
///   (UNLOCKED > DEGRADED > LOCKED); a LOCKED-but-clock-not-ok output is demoted
///   to UNLOCKED. The reason is the worst live output's reason (or `"locked"`).
/// - **Pacing enabled but nothing live** → derived from the enabled set's clock
///   only: LOCKED iff every enabled output's clock is ok, else UNLOCKED
///   (mirrors [`summarize`]'s no-live branch).
pub fn global_lock_summary(outputs: &[GlobalLockInput]) -> GlobalLockSummary {
    let enabled: Vec<&GlobalLockInput> = outputs.iter().filter(|o| o.pacing_enabled).collect();
    if enabled.is_empty() {
        return GlobalLockSummary {
            state: GlobalLock::Off,
            reason: OFF_REASON.to_string(),
        };
    }

    let live: Vec<&GlobalLockInput> = enabled.iter().copied().filter(|o| o.live).collect();
    if live.is_empty() {
        let clock_ok = enabled.iter().all(|o| o.clock_ok);
        return if clock_ok {
            GlobalLockSummary {
                state: GlobalLock::Locked,
                reason: "no live output".to_string(),
            }
        } else {
            GlobalLockSummary {
                state: GlobalLock::Unlocked,
                reason: "clock not ok".to_string(),
            }
        };
    }

    let worst = live
        .iter()
        .copied()
        .max_by_key(|o| severity(effective_global(o)))
        .expect("live is non-empty");
    let (state, reason) = match effective_global(worst) {
        LockState::Locked => (GlobalLock::Locked, "locked".to_string()),
        LockState::Degraded => (GlobalLock::Degraded, worst.reason.clone()),
        LockState::Unlocked => (GlobalLock::Unlocked, worst.reason.clone()),
    };
    GlobalLockSummary { state, reason }
}
