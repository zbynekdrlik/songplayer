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
