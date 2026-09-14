//! Mutation-kill tests for `genlock::lock_state::severity` (PR #153 / 0.47.0).
//!
//! `severity` is private and used only by `summarize`'s `max_by_key`, so it is
//! exercised through `summarize`. The two surviving mutants replace the whole
//! `severity` body with the constant `0` or `1`, flattening the ordering. With
//! all severities equal, `max_by_key` keeps the LAST element — so ordering the
//! outputs worst-FIRST makes the flattened pick differ from the true worst.
//! Wired from `genlock.rs`; references the public `lock_state` API.

use crate::genlock::lock_state::{LockState, LockSummary, OutputLock, summarize};

fn ol(name: &str, state: LockState) -> OutputLock {
    OutputLock {
        name: name.to_string(),
        state,
        live: true,
        clock_ok: true,
    }
}

#[test]
fn summarize_picks_unlocked_over_degraded_over_locked_when_worst_is_first() {
    // Worst (Unlocked) placed FIRST. True severity picks it (== Some("c")).
    // A flattened severity (const 0 or 1) makes max_by_key keep the LAST
    // element (Locked "a"), which summarizes to LOCKED / worst = None.
    let outputs = [
        ol("c", LockState::Unlocked),
        ol("b", LockState::Degraded),
        ol("a", LockState::Locked),
    ];
    let s = summarize(&outputs);
    assert_eq!(
        s,
        LockSummary {
            state: LockState::Unlocked,
            worst: Some("c".to_string()),
            live_count: 3,
        },
        "worst live state (Unlocked) must win the summary regardless of position"
    );
}

#[test]
fn summarize_ranks_degraded_worse_than_locked() {
    // Degraded first, Locked last. const-0/1 severity would keep Locked "z"
    // (last) -> LOCKED/None; true severity keeps Degraded "d".
    let outputs = [ol("d", LockState::Degraded), ol("z", LockState::Locked)];
    let s = summarize(&outputs);
    assert_eq!(
        s,
        LockSummary {
            state: LockState::Degraded,
            worst: Some("d".to_string()),
            live_count: 2,
        },
        "Degraded must outrank Locked (severity 1 > 0)"
    );
}
