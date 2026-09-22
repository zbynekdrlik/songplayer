//! #184 round G0.1 — dub-priority yield decisions for the stem worker.
//!
//! A dub job is an explicit operator request with a deadline; a background stem
//! separation is not. When a dub is queued behind the process-global heavy slot
//! (`heavy_slot::dub_slot_wanted()`), the stem worker (a) DEFERS its next tick
//! ([`stem_tick_defers_to_dub`]) and (b) YIELDS a separation already running
//! ([`run_with_dub_yield`]), so the dub acquires within ~1 s.
//!
//! The pure decisions [`stem_tick_defers_to_dub`] / [`yield_reason`] are unit-
//! tested exactly; [`run_with_dub_yield`] is timing orchestration only
//! (`mutants::skip`, mirroring `idle_gate_abort::run_with_wall_abort`) and reuses
//! the tested `AbortPolicy` debounce for the wall arm.

use crate::lyrics::heavy_plan::HeavyStepPlan;
use crate::lyrics::heavy_slot::dub_slot_wanted;
use crate::lyrics::idle_gate::WallActivity;
use crate::lyrics::idle_gate_abort::{ABORT_POLL_INTERVAL, AbortPolicy};

/// Pure: whether a stem worker tick must be skipped because a dub is queued
/// behind the heavy slot. `true` while a dub waits, so the tick starts no new
/// separation (the row stays pending — no backoff, no DB write). The lyrics
/// worker's heavy tick defers on the same flag.
pub(crate) fn stem_tick_defers_to_dub(dub_wanted: bool) -> bool {
    dub_wanted
}

/// Why a running heavy separation must yield the process-global heavy slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Yield {
    /// A dub job is queued behind the slot — preempts ANY plan (an explicit
    /// operator request with a deadline).
    Dub,
    /// The live wall went busy — preempts a GPU plan only (the #161 rule); a
    /// cpu-idle separation cannot disturb the wall, so it is never wall-yielded.
    Wall,
}

/// Pure: should a running separation yield the heavy slot NOW, and why? A queued
/// dub preempts ANY plan; a busy wall preempts a GPU plan only. A dub wins if
/// both. `None` → keep running. Exhaustively unit-tested.
pub(crate) fn yield_reason(
    dub_wanted: bool,
    wall_busy: bool,
    plan: &HeavyStepPlan,
) -> Option<Yield> {
    // RED (#184 G0.1): reversed precedence — the wall arm is checked BEFORE the
    // dub arm, so a dub queued behind a busy-wall GPU separation wrongly yields to
    // the wall instead of the dub. GREEN swaps the order back so the dub wins.
    if wall_busy && plan.is_gpu() {
        Some(Yield::Wall)
    } else if dub_wanted {
        Some(Yield::Dub)
    } else {
        None
    }
}

/// The abort a running separation returns when it yields the slot. `detail` names
/// the cause for the INFO log / dashboard.
#[derive(Debug)]
pub(crate) struct YieldAbort {
    pub(crate) reason: Yield,
    pub(crate) detail: String,
}

/// The outcome of one watched separation attempt.
pub(crate) enum SepResult {
    Done,
    Failed(anyhow::Error),
    Yielded { reason: Yield, detail: String },
}

/// Race `fut` (a separation subprocess future with `kill_on_drop`) against a 1 s
/// poll that yields the heavy slot to a queued dub (ANY plan, no debounce) or to
/// a busy wall (GPU plan only, after the #161 2-consecutive-busy debounce).
/// Returns `Ok(output)` if it finishes first, else `Err(YieldAbort)` — dropping
/// `fut`, which SIGKILLs the child, so the dub acquires within ~1 s. `dub_wanted`
/// is injected (the production caller passes `heavy_slot::dub_slot_wanted`); the
/// decision is the pure [`yield_reason`], this is orchestration only.
#[cfg_attr(test, mutants::skip)]
pub(crate) async fn run_with_dub_yield<T, D, W, Fut>(
    fut: impl std::future::Future<Output = T>,
    plan: HeavyStepPlan,
    dub_wanted: D,
    mut wall: W,
) -> Result<T, YieldAbort>
where
    D: Fn() -> bool,
    W: FnMut() -> Fut,
    Fut: std::future::Future<Output = WallActivity>,
{
    tokio::pin!(fut);
    let mut policy = AbortPolicy::default();
    let mut ticker = tokio::time::interval(ABORT_POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // `interval`'s first tick fires immediately; consume it so the first real
    // sample is one interval into the run.
    ticker.tick().await;
    loop {
        tokio::select! {
            out = &mut fut => return Ok(out),
            _ = ticker.tick() => {
                let activity = wall().await;
                // Debounce the wall arm exactly like #161 (2 consecutive busy);
                // the dub arm is immediate (checked fresh each poll).
                let wall_busy = policy.observe(true, activity.in_use());
                if let Some(reason) = yield_reason(dub_wanted(), wall_busy, &plan) {
                    let detail = match reason {
                        Yield::Dub => "dub job waiting for the heavy slot".to_string(),
                        Yield::Wall => activity.reason().unwrap_or("wall in use").to_string(),
                    };
                    return Err(YieldAbort { reason, detail });
                }
            }
        }
    }
}

/// Run one separation `sep` under [`run_with_dub_yield`] (reading the live dub
/// flag) and map its result to a [`SepResult`]. Thin mapper — the decision is
/// `yield_reason`.
#[cfg_attr(test, mutants::skip)]
pub(crate) async fn run_separation_watched<W, Fut>(
    sep: impl std::future::Future<Output = anyhow::Result<()>>,
    plan: HeavyStepPlan,
    wall: W,
) -> SepResult
where
    W: FnMut() -> Fut,
    Fut: std::future::Future<Output = WallActivity>,
{
    match run_with_dub_yield(sep, plan, dub_slot_wanted, wall).await {
        Ok(Ok(())) => SepResult::Done,
        Ok(Err(e)) => SepResult::Failed(e),
        Err(y) => SepResult::Yielded {
            reason: y.reason,
            detail: y.detail,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    fn busy() -> WallActivity {
        WallActivity {
            any_playing: true,
            obs_streaming: false,
            obs_recording: false,
            known: true,
        }
    }

    fn idle() -> WallActivity {
        WallActivity::default()
    }

    // ---- stem_tick_defers_to_dub (pure) ----------------------------------

    #[test]
    fn a_queued_dub_defers_a_stem_tick() {
        assert!(
            stem_tick_defers_to_dub(true),
            "a dub queued behind the heavy slot defers the stem tick"
        );
        assert!(
            !stem_tick_defers_to_dub(false),
            "no dub queued → the stem worker proceeds"
        );
    }

    // ---- yield_reason (pure) — every branch, exact values ----------------

    #[test]
    fn yield_reason_dub_wins_any_plan() {
        // A queued dub preempts ANY plan (design acceptance #1).
        assert_eq!(
            yield_reason(true, false, &HeavyStepPlan::cpu_idle()),
            Some(Yield::Dub),
            "a dub preempts a cpu-idle separation"
        );
        assert_eq!(
            yield_reason(true, false, &HeavyStepPlan::gpu_below_normal()),
            Some(Yield::Dub)
        );
        // Dub wins even when the wall is ALSO busy on a GPU plan (precedence).
        assert_eq!(
            yield_reason(true, true, &HeavyStepPlan::gpu_below_normal()),
            Some(Yield::Dub),
            "a dub wins over a busy wall"
        );
        assert_eq!(
            yield_reason(true, true, &HeavyStepPlan::cpu_idle()),
            Some(Yield::Dub)
        );
    }

    #[test]
    fn yield_reason_wall_only_for_a_gpu_plan() {
        // A busy wall preempts a GPU plan (today's #161 rule)...
        assert_eq!(
            yield_reason(false, true, &HeavyStepPlan::gpu_below_normal()),
            Some(Yield::Wall)
        );
        // ...but never a cpu-idle plan (it cannot disturb the wall).
        assert_eq!(
            yield_reason(false, true, &HeavyStepPlan::cpu_idle()),
            None,
            "a cpu-idle separation is never wall-yielded"
        );
    }

    #[test]
    fn yield_reason_neither_keeps_running() {
        assert_eq!(
            yield_reason(false, false, &HeavyStepPlan::gpu_below_normal()),
            None
        );
        assert_eq!(yield_reason(false, false, &HeavyStepPlan::cpu_idle()), None);
    }

    // ---- run_with_dub_yield (orchestration; dub_wanted injected) ----------

    #[tokio::test(start_paused = true)]
    async fn run_with_dub_yield_yields_a_running_separation_to_a_queued_dub() {
        let completed = Arc::new(AtomicBool::new(false));
        let c = completed.clone();
        // A "heavy" separation: sets `completed` only if it runs to the end. On a
        // dub-yield it is DROPPED (kill_on_drop) before the sleep elapses.
        let sep = async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            c.store(true, Ordering::SeqCst);
            Ok::<(), anyhow::Error>(())
        };
        // Wall IDLE + a cpu-idle plan (never wall-yielded): the yield is due to
        // the DUB alone (`dub_wanted = || true`).
        let result =
            run_with_dub_yield(sep, HeavyStepPlan::cpu_idle(), || true, || async { idle() }).await;
        assert!(
            matches!(
                result,
                Err(YieldAbort {
                    reason: Yield::Dub,
                    ..
                })
            ),
            "a queued dub must yield even a cpu-idle separation: {result:?}"
        );
        assert!(
            !completed.load(Ordering::SeqCst),
            "the yielded separation future must be dropped, not awaited to completion"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn run_with_dub_yield_no_dub_lets_a_cpu_separation_finish_even_on_a_busy_wall() {
        let sep = async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok::<(), anyhow::Error>(())
        };
        // No dub + cpu-idle plan + permanently busy wall → never yields (a
        // cpu-idle separation cannot disturb the wall).
        let result = run_with_dub_yield(
            sep,
            HeavyStepPlan::cpu_idle(),
            || false,
            || async { busy() },
        )
        .await;
        assert!(
            matches!(result, Ok(Ok(()))),
            "no dub + cpu plan must run to completion: {result:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn run_with_dub_yield_wall_yields_a_gpu_separation_after_the_debounce() {
        let completed = Arc::new(AtomicBool::new(false));
        let c = completed.clone();
        let sep = async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            c.store(true, Ordering::SeqCst);
            Ok::<(), anyhow::Error>(())
        };
        // No dub, GPU plan, permanently busy wall → wall-yields after the 2 s
        // debounce, exactly like #161.
        let result = run_with_dub_yield(
            sep,
            HeavyStepPlan::gpu_below_normal(),
            || false,
            || async { busy() },
        )
        .await;
        assert!(
            matches!(
                result,
                Err(YieldAbort {
                    reason: Yield::Wall,
                    ..
                })
            ),
            "a busy wall must yield a GPU separation: {result:?}"
        );
        assert!(!completed.load(Ordering::SeqCst));
    }
}
