//! Runtime burn-id overlay toggle registry (#151).
//!
//! Maps each pipeline's NDI output name to its shared burn flag +
//! `genlock_pacing` status. `POST /api/v1/ndi/burn` writes it SYNCHRONOUSLY
//! (404 unknown output / 409 pacing disabled / 204 ok); the paced
//! `FrameSubmitter` reads the SAME `Arc<AtomicBool>` on every boundary emit (so
//! a toggle takes effect within one frame); the engine reads it when building
//! each health snapshot (`burn_on`). Default OFF, never persisted — the registry
//! is rebuilt at pipeline spawn, so a restart always clears the burn (a QR must
//! never survive onto the LED wall). Mirrors the `NdiHealthRegistry` /
//! `ResolumeRegistry` shared-registry pattern for API↔engine state.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

/// Outcome of a `POST /api/v1/ndi/burn` toggle attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BurnSetResult {
    /// Flag updated — respond `204 No Content`.
    Ok,
    /// No pipeline with that NDI output name — respond `404 Not Found`.
    NotFound,
    /// The output exists but `genlock_pacing` is off — respond `409 Conflict`
    /// ("pacing disabled"). A burn is only painted on the paced path.
    PacingDisabled,
}

struct BurnEntry {
    flag: Arc<AtomicBool>,
    paced: bool,
}

/// Shared registry of per-output burn flags.
pub struct NdiBurnRegistry {
    entries: RwLock<HashMap<String, BurnEntry>>,
}

impl NdiBurnRegistry {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
        }
    }

    /// Register (or re-register) a pipeline at spawn, returning the shared burn
    /// flag for its `FrameSubmitter`. The flag ALWAYS starts OFF (never
    /// persisted); `paced` is the pipeline's `genlock_pacing` status.
    pub fn register(&self, ndi_name: &str, paced: bool) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        if let Ok(mut map) = self.entries.write() {
            map.insert(
                ndi_name.to_string(),
                BurnEntry {
                    flag: flag.clone(),
                    paced,
                },
            );
        }
        flag
    }

    /// Remove a pipeline's entry (pipeline torn down).
    pub fn unregister(&self, ndi_name: &str) {
        if let Ok(mut map) = self.entries.write() {
            map.remove(ndi_name);
        }
    }

    /// Set the burn flag for `ndi_name`. `NotFound` if unknown, `PacingDisabled`
    /// if the pipeline is not paced.
    pub fn set(&self, ndi_name: &str, on: bool) -> BurnSetResult {
        let map = match self.entries.read() {
            Ok(m) => m,
            Err(_) => return BurnSetResult::NotFound,
        };
        match map.get(ndi_name) {
            None => BurnSetResult::NotFound,
            Some(e) if !e.paced => BurnSetResult::PacingDisabled,
            Some(e) => {
                e.flag.store(on, Ordering::Relaxed);
                BurnSetResult::Ok
            }
        }
    }

    /// Current burn state for `ndi_name` (false if unknown) — for the health
    /// snapshot's `burn_on`.
    pub fn is_on(&self, ndi_name: &str) -> bool {
        self.entries
            .read()
            .ok()
            .and_then(|m| m.get(ndi_name).map(|e| e.flag.load(Ordering::Relaxed)))
            .unwrap_or(false)
    }
}

impl Default for NdiBurnRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_output_is_not_found() {
        let r = NdiBurnRegistry::new();
        assert_eq!(r.set("nope", true), BurnSetResult::NotFound);
        assert!(!r.is_on("nope"));
    }

    #[test]
    fn non_paced_output_rejects_with_pacing_disabled() {
        let r = NdiBurnRegistry::new();
        r.register("SP-legacy", false);
        assert_eq!(r.set("SP-legacy", true), BurnSetResult::PacingDisabled);
        assert!(!r.is_on("SP-legacy"), "a 409 must NOT flip the flag");
    }

    #[test]
    fn paced_output_toggles_and_shares_the_flag() {
        let r = NdiBurnRegistry::new();
        let submitter_flag = r.register("SP-fast", true);
        assert!(!submitter_flag.load(Ordering::Relaxed), "default OFF");
        assert!(!r.is_on("SP-fast"));

        assert_eq!(r.set("SP-fast", true), BurnSetResult::Ok);
        assert!(
            submitter_flag.load(Ordering::Relaxed),
            "the submitter's shared flag reflects the API toggle within one frame"
        );
        assert!(r.is_on("SP-fast"));

        assert_eq!(r.set("SP-fast", false), BurnSetResult::Ok);
        assert!(!submitter_flag.load(Ordering::Relaxed));
        assert!(!r.is_on("SP-fast"));
    }

    #[test]
    fn re_register_clears_the_flag_never_persisted() {
        let r = NdiBurnRegistry::new();
        let f1 = r.register("SP-fast", true);
        r.set("SP-fast", true);
        assert!(f1.load(Ordering::Relaxed));
        // A restart re-registers -> a fresh OFF flag (a QR never survives a restart).
        let f2 = r.register("SP-fast", true);
        assert!(!f2.load(Ordering::Relaxed));
        assert!(!r.is_on("SP-fast"));
    }

    #[test]
    fn unregister_removes_the_output() {
        let r = NdiBurnRegistry::new();
        r.register("SP-fast", true);
        r.unregister("SP-fast");
        assert_eq!(r.set("SP-fast", true), BurnSetResult::NotFound);
    }
}
