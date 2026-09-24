//! #162 pure tests for the box-overload guard: the memory-headroom decision
//! core and the process-global heavy-step slot serialization. Never reads real
//! memory (the headroom is injected) and never touches the DB.

use super::*;
// `super::*` does NOT re-export the parent's private `use containment_from_settings`
// alias, so name it explicitly for the #207 contained-line test (rust-workspace.md).
use crate::lyrics::heavy_containment::containment_from_settings;

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
// explicit; both call the identical gate `memory_ok_for`.

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

// ---- #144 r2: measure headroom AT SPAWN, inside the slot -----------------
// `acquire_on_checked` queues on the slot FIRST (fair FIFO — the caller blocks
// behind a running heavy child), then measures the injected headroom with the
// permit HELD. This is the flipped admission order: a pre-slot reading measured
// the very child the slot serialises away, so from #168 r3 the lyrics worker
// never queued and the #162 FIFO alternation was dead. Injected `Arc<Semaphore>`
// + injected reader — no process-global state, no real memory read.

#[tokio::test(start_paused = true)]
async fn acquire_for_spawn_queues_behind_a_running_child_then_admits() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tokio::sync::Semaphore;

    let slot = Arc::new(Semaphore::new(1));
    // Another heavy child holds the only permit (a running separation).
    let holder = acquire_on(slot.clone(), "running").await;

    // The spawn-time reading is LOW while that child is resident (it pins
    // commit) and OK once it exits — exactly the #144 r2 condition.
    let child_gone = Arc::new(AtomicBool::new(false));
    let cg = child_gone.clone();
    let read = move || {
        if cg.load(Ordering::SeqCst) {
            Some(Headroom {
                free_phys: 8 * GIB,
                free_commit: 8 * GIB,
            })
        } else {
            Some(Headroom {
                free_phys: 8 * GIB,
                free_commit: 2 * GIB,
            })
        }
    };

    let fut = acquire_on_checked(slot.clone(), "isolation", read);
    tokio::pin!(fut);

    // While the child holds the permit the queued acquire CANNOT complete — it
    // is parked in the FIFO behind the running child, not spinning on the
    // reading. (Under `start_paused` the runtime auto-advances the timer while
    // the only task is parked on the semaphore.)
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut fut)
            .await
            .is_err(),
        "acquire_on_checked must queue behind the running child, not return early"
    );

    // The child exits: release the permit AND the reading flips OK. The queued
    // acquire now admits with the permit held.
    child_gone.store(true, Ordering::SeqCst);
    drop(holder);
    let guard = fut
        .await
        .expect("admits once the slot frees and spawn-time headroom is ok");
    drop(guard);
}

#[tokio::test]
async fn acquire_for_spawn_releases_the_permit_on_a_low_reading() {
    use std::sync::Arc;
    use tokio::sync::Semaphore;

    let slot = Arc::new(Semaphore::new(1));
    // The permit is FREE, but the spawn-time reading is low → defer.
    let low = || {
        Some(Headroom {
            free_phys: 8 * GIB,
            free_commit: 2 * GIB,
        })
    };
    let r = acquire_on_checked(slot.clone(), "stem separation", low).await;
    assert!(
        matches!(r, Err(HeadroomLow)),
        "a low spawn-time reading defers with Err(HeadroomLow)"
    );

    // The failed attempt must have RELEASED the permit (it never entered the
    // step): the slot is free again and a fresh acquire gets it immediately —
    // no deadlock, exactly like the old pre-slot check that never joined the
    // FIFO.
    assert_eq!(
        slot.available_permits(),
        1,
        "the deferred attempt released the permit"
    );
    let _g = acquire_on(slot.clone(), "next").await; // must not block
}

// ---- #184 G0.1: dub-priority slot flag ----------------------------------

#[test]
fn acquire_clears_dub_want_only_for_the_dub_step() {
    // Only the dub step's acquire clears the flag; a stem / isolation / mtl
    // acquire must NOT (a queued dub is still waiting behind them).
    assert!(acquire_clears_dub_want("dub live-translate"));
    assert!(!acquire_clears_dub_want("stem separation"));
    assert!(!acquire_clears_dub_want("isolation"));
    assert!(!acquire_clears_dub_want("mtl align"));
    // Pinned to the exact name the dub's `acquire_slot_for_spawn` uses.
    assert_eq!(DUB_STEP_NAME, "dub live-translate");
}

#[tokio::test]
async fn dub_slot_want_guard_sets_true_and_drop_clears() {
    let _lk = DUB_FLAG_SERIAL.lock().await;
    set_dub_slot_wanted(false);
    assert!(!dub_slot_wanted(), "the flag starts clear");
    {
        let _want = dub_slot_want_guard();
        assert!(
            dub_slot_wanted(),
            "the guard publishes the flag while a dub is queued"
        );
    }
    assert!(
        !dub_slot_wanted(),
        "the guard's Drop clears the flag (early-return safety net)"
    );
    set_dub_slot_wanted(false);
}

/// Acceptance #1 fake-timeline test (injected local `Arc<Semaphore>`): the flag
/// is TRUE between "a dub is queued" and "the dub acquires the slot", and FALSE
/// the instant the DUB step acquires; a non-dub acquire never clears it.
#[tokio::test]
async fn dub_acquire_clears_the_flag_but_a_stem_acquire_does_not() {
    use std::sync::Arc;
    use tokio::sync::Semaphore;

    let _lk = DUB_FLAG_SERIAL.lock().await;
    let slot = Arc::new(Semaphore::new(1));

    // Job picked, slot not yet acquired → flag TRUE.
    set_dub_slot_wanted(false);
    let _want = dub_slot_want_guard();
    assert!(
        dub_slot_wanted(),
        "TRUE while the dub is queued behind the slot"
    );

    // The DUB step's acquire clears it the instant it acquires → FALSE after.
    let g = acquire_on(slot.clone(), DUB_STEP_NAME).await;
    assert!(
        !dub_slot_wanted(),
        "the dub's acquire clears the flag (FALSE right after acquiring)"
    );
    drop(g);

    // A non-dub (stem) acquire must leave a queued dub's flag alone.
    set_dub_slot_wanted(true);
    let g2 = acquire_on(slot.clone(), "stem separation").await;
    assert!(
        dub_slot_wanted(),
        "a stem acquire must not clear a queued dub's flag"
    );
    drop(g2);
    set_dub_slot_wanted(false);
}

// ---- pinned constants (literals; the cfg(windows) product was un-mutatable) ----

#[test]
fn child_job_limit_is_ten_gib() {
    assert_eq!(CHILD_JOB_MEMORY_LIMIT_BYTES, 10 * (1u64 << 30));
    assert_eq!(HEAVY_STEP_MIN_FREE_BYTES, 4 * (1u64 << 30));
    const {
        assert!(
            CHILD_JOB_MEMORY_LIMIT_BYTES > HEAVY_STEP_MIN_FREE_BYTES,
            "a child may use more than the admission floor, never less"
        );
    }
}

// ---- contained_line (pure #207 formatter, exact-string) --------------------

/// The `heavy child contained` line is grep-stable and carries every applied
/// field — cpu cap, affinity (lowercase hex), memory priority, purge delay, and
/// (#207 phase-3) `alloc_mode=` after `purge_delay_ms=`. Distinct field values
/// so an interpolation-swap mutant diverges.
#[test]
fn contained_line_is_grep_stable_with_alloc_mode() {
    // cap 25, mask 0xE00000 (top-3 of 24), purge 1000, alloc lazy, reserve absent (default 4).
    let c = containment_from_settings(
        Some("25"),
        Some("e00000"),
        Some("1000"),
        Some("lazy"),
        None,
        None,
        24,
    );
    assert_eq!(
        contained_line(42, 1_073_741_824, &c),
        "heavy child contained (pid 42): mem_limit=1073741824B cpu_cap=25% affinity=0xe00000 mem_priority_low=true purge_delay_ms=1000 alloc_mode=lazy reserve_gib=4 max_ws_mb=4096"
    );
    // The default (retained) mode renders `alloc_mode=retained`.
    let c = containment_from_settings(Some("50"), Some("f0"), None, None, None, None, 8);
    assert_eq!(
        contained_line(7, 2048, &c),
        "heavy child contained (pid 7): mem_limit=2048B cpu_cap=50% affinity=0xf0 mem_priority_low=true purge_delay_ms=-1 alloc_mode=retained reserve_gib=4 max_ws_mb=4096"
    );
}

/// #207 round-3c: the contained line gains ` reserve_gib=<n>` after
/// `alloc_mode=` — the operator `heavy_alloc_reserve_gib` size (GiB) applied
/// to the separation child's `MIMALLOC_RESERVE_OS_MEMORY` at spawn.
#[test]
fn contained_line_carries_reserve_gib_after_alloc_mode() {
    let c = containment_from_settings(
        Some("25"),
        Some("e00000"),
        Some("1000"),
        Some("lazy"),
        Some("2"),
        None,
        24,
    );
    assert_eq!(
        contained_line(42, 1_073_741_824, &c),
        "heavy child contained (pid 42): mem_limit=1073741824B cpu_cap=25% affinity=0xe00000 mem_priority_low=true purge_delay_ms=1000 alloc_mode=lazy reserve_gib=2 max_ws_mb=4096"
    );
}

// ---- #147 round 9 — the heavy child's working-set cap ----------------------

/// The contained line ends with ` max_ws_mb=<n>` — the cap the Job Object
/// APPLIED — or ` max_ws_mb=off` when the cap is disabled (`0`) or was
/// rejected by the OS (the seam zeroes it on the retry-without path).
#[test]
fn contained_line_carries_the_applied_working_set_cap() {
    let c = containment_from_settings(None, Some("e00000"), None, None, None, Some("2048"), 24);
    assert_eq!(
        contained_line(9, 4096, &c),
        "heavy child contained (pid 9): mem_limit=4096B cpu_cap=25% affinity=0xe00000 mem_priority_low=true purge_delay_ms=-1 alloc_mode=retained reserve_gib=4 max_ws_mb=2048"
    );
    let off = containment_from_settings(None, Some("e00000"), None, None, None, Some("0"), 24);
    assert_eq!(
        contained_line(9, 4096, &off),
        "heavy child contained (pid 9): mem_limit=4096B cpu_cap=25% affinity=0xe00000 mem_priority_low=true purge_delay_ms=-1 alloc_mode=retained reserve_gib=4 max_ws_mb=off"
    );
}

/// `resolve_containment` reads `heavy_max_working_set_mb` — the SAME key
/// `PATCH /api/v1/settings` writes (the settings API stores any key) — so a
/// PATCH takes effect at the next heavy-child spawn. It does NOT publish, so
/// this test never disturbs the process-global snapshot other tests read.
#[tokio::test]
async fn resolve_containment_reads_the_patched_working_set_cap() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    assert_eq!(
        resolve_containment(&pool).await.max_working_set_mb,
        4096,
        "absent → the 4096 MiB default"
    );
    for (value, want) in [
        ("2048", 2048),
        ("0", 0),
        ("100", 512),
        ("99999", 10_240),
        ("x", 4096),
    ] {
        crate::db::models::set_setting(&pool, "heavy_max_working_set_mb", value)
            .await
            .unwrap();
        assert_eq!(
            resolve_containment(&pool).await.max_working_set_mb,
            want,
            "{value}"
        );
    }
}
