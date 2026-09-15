//! #162 pure tests for the box-overload guard: the memory-headroom decision
//! core and the process-global heavy-step slot serialization. Never reads real
//! memory (the headroom is injected) and never touches the DB.

use super::*;

const GIB: u64 = 1024 * 1024 * 1024;

// ---- headroom_ok (pure) --------------------------------------------------

#[test]
fn headroom_ok_requires_both_at_or_above_min() {
    // Both above → ok.
    assert!(headroom_ok(8 * GIB, 8 * GIB, 4 * GIB));
    // Exactly at the threshold on both → ok (>=, not >).
    assert!(headroom_ok(4 * GIB, 4 * GIB, 4 * GIB));
    // Free physical below → not ok (this is the 07:40 case: low phys, ample commit).
    assert!(!headroom_ok(2 * GIB, 8 * GIB, 4 * GIB));
    // Free commit below → not ok.
    assert!(!headroom_ok(8 * GIB, 2 * GIB, 4 * GIB));
    // One byte below on physical → not ok.
    assert!(!headroom_ok(4 * GIB - 1, 8 * GIB, 4 * GIB));
}

// ---- admission_from_headroom (pure, uses HEAVY_STEP_MIN_FREE_BYTES) -------

#[test]
fn admission_none_reading_allows() {
    // Unknown reading (non-Windows / read failed) must never wedge the queue.
    assert_eq!(admission_from_headroom(None), MemoryAdmission::Ok);
}

#[test]
fn admission_ample_headroom_allows() {
    let r = Headroom {
        free_phys: 8 * GIB,
        free_commit: 8 * GIB,
    };
    assert_eq!(admission_from_headroom(Some(r)), MemoryAdmission::Ok);
}

#[test]
fn admission_low_physical_defers_with_numbers() {
    let r = Headroom {
        free_phys: 2 * GIB,
        free_commit: 8 * GIB,
    };
    assert_eq!(
        admission_from_headroom(Some(r)),
        MemoryAdmission::Defer {
            free_phys: 2 * GIB,
            free_commit: 8 * GIB,
        }
    );
}

#[test]
fn admission_low_commit_defers() {
    let r = Headroom {
        free_phys: 8 * GIB,
        free_commit: GIB,
    };
    assert!(matches!(
        admission_from_headroom(Some(r)),
        MemoryAdmission::Defer { .. }
    ));
}

// ---- memory_ok_for — the worker-level gate BOTH workers call -------------
// Injecting a low headroom reading proves the deferral without reading real
// memory. Named per worker so the "lyrics + stems both defer" acceptance is
// explicit; both call the identical gate via `heavy_step_memory_ok`.

#[test]
fn lyrics_isolation_defers_when_headroom_low() {
    let low = Headroom {
        free_phys: 2 * GIB,
        free_commit: 8 * GIB,
    };
    assert!(
        !memory_ok_for("isolation", Some(low)),
        "lyrics isolation must defer (no penalty) when free RAM is below 4 GiB"
    );
    // Ample headroom → the step proceeds.
    let ample = Headroom {
        free_phys: 8 * GIB,
        free_commit: 8 * GIB,
    };
    assert!(memory_ok_for("isolation", Some(ample)));
    // Unknown reading → proceeds (allow).
    assert!(memory_ok_for("isolation", None));
}

#[test]
fn stem_separation_defers_when_headroom_low() {
    let low = Headroom {
        free_phys: 8 * GIB,
        free_commit: 2 * GIB,
    };
    assert!(
        !memory_ok_for("stem separation", Some(low)),
        "stem separation must defer (no penalty) when free commit is below 4 GiB"
    );
    let ample = Headroom {
        free_phys: 8 * GIB,
        free_commit: 8 * GIB,
    };
    assert!(memory_ok_for("stem separation", Some(ample)));
}

// ---- heavy-step slot serialization (process-wide "one at a time") --------

#[tokio::test]
async fn slot_serializes_two_heavy_steps() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::Semaphore;

    let slot = Arc::new(Semaphore::new(1));

    // First heavy step holds the only permit.
    let g1 = acquire_on(slot.clone(), "first").await;

    // Second heavy step tries to start while the first holds the slot.
    let started = Arc::new(AtomicBool::new(false));
    let started2 = started.clone();
    let slot2 = slot.clone();
    let handle = tokio::spawn(async move {
        let _g2 = acquire_on(slot2, "second").await;
        started2.store(true, Ordering::SeqCst);
    });

    // Give the spawned task several chances to run; it must PARK on the slot,
    // never acquire, while the first step holds it.
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert!(
        !started.load(Ordering::SeqCst),
        "second heavy step ran while the first still held the slot"
    );

    // Release the first → the second acquires and completes.
    drop(g1);
    handle.await.unwrap();
    assert!(
        started.load(Ordering::SeqCst),
        "second heavy step never ran after the first released the slot"
    );
}

// ---- pinned constants (literals; the cfg(windows) product was un-mutatable) ----

#[test]
fn child_job_limit_is_six_gib() {
    assert_eq!(CHILD_JOB_MEMORY_LIMIT_BYTES, 6 * (1u64 << 30));
    assert_eq!(HEAVY_STEP_MIN_FREE_BYTES, 4 * (1u64 << 30));
    assert!(
        CHILD_JOB_MEMORY_LIMIT_BYTES > HEAVY_STEP_MIN_FREE_BYTES,
        "a child may use more than the admission floor, never less"
    );
}
