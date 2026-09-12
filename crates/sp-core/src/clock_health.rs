//! dantesync clock-health vocabulary (#146, contract §D/§7).
//!
//! The shared, WASM-safe types plus the pure reduction ([`evaluate`]) that the
//! NDI health snapshot and (in #150) the WASM dashboard badge both speak.
//! Kept in `sp-core` so the vocabulary is one definition; the 1 Hz poller that
//! feeds it lives in `sp-server` (`playback::clock_health`) with the reqwest /
//! tokio I/O, and re-exports these types.
//!
//! `clock_ok = is_locked && mode ∈ {LOCK, NANO}`. A missing/unreachable
//! endpoint yields `clock_ok = false` with reason `"no dantesync"` and MUST
//! never block playback (contract §4).

use serde::{Deserialize, Serialize};

/// Reduced clock health exposed on the NDI health snapshot + dashboard.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ClockHealth {
    pub is_locked: bool,
    pub mode: String,
    pub offset_ns: Option<i64>,
    pub ntp_failed: Option<bool>,
    pub ntp_age_s: Option<i64>,
    /// `is_locked && mode ∈ {LOCK, NANO}` — the CLOCK-OK precondition.
    pub clock_ok: bool,
    /// Human reason when `!clock_ok` (e.g. `"no dantesync"`).
    pub reason: Option<String>,
    /// RFC-3339 timestamp of the poll that produced this value; `None` for the
    /// default/never-polled state.
    pub sampled_at: Option<String>,
}

impl Default for ClockHealth {
    fn default() -> Self {
        evaluate(None)
    }
}

/// Raw dantesync `/status` payload. Every field is optional so a partial or
/// older payload still deserialises; unknown fields are ignored by serde.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct DantesyncStatus {
    #[serde(default)]
    pub is_locked: Option<bool>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub offset_ns: Option<i64>,
    #[serde(default)]
    pub ntp_failed: Option<bool>,
    #[serde(default)]
    pub ntp_age_s: Option<i64>,
}

/// Pure reduction of a dantesync payload to [`ClockHealth`]. `None` (endpoint
/// missing/unreachable/unparseable) → not ok, reason `"no dantesync"`.
pub fn evaluate(payload: Option<&DantesyncStatus>) -> ClockHealth {
    match payload {
        None => ClockHealth {
            is_locked: false,
            mode: String::new(),
            offset_ns: None,
            ntp_failed: None,
            ntp_age_s: None,
            clock_ok: false,
            reason: Some("no dantesync".to_string()),
            sampled_at: None,
        },
        Some(s) => {
            let is_locked = s.is_locked.unwrap_or(false);
            let mode = s.mode.clone().unwrap_or_default();
            let clock_ok = is_locked && (mode == "LOCK" || mode == "NANO");
            let reason = if clock_ok {
                None
            } else {
                Some(format!(
                    "clock not ok (is_locked={is_locked}, mode={mode:?})"
                ))
            };
            ClockHealth {
                is_locked,
                mode,
                offset_ns: s.offset_ns,
                ntp_failed: s.ntp_failed,
                ntp_age_s: s.ntp_age_s,
                clock_ok,
                reason,
                sampled_at: None,
            }
        }
    }
}

#[cfg(test)]
#[path = "clock_health_tests.rs"]
mod clock_health_tests;
