//! LIVE-LOCKED genlock indicator (#150).
//!
//! Renders the per-output + global lock badges from `GET /api/v1/ndi/health`:
//! per-output `lock_state` / `lock_reason` (#149), `clock` (#146),
//! `pacing` (#147), `audio` (#148). Same LOCKED / DEGRADED / UNLOCKED
//! vocabulary + colours as the fleet OBS indicator (camera-box#1298).
//! Indication only — no sound, no modal, no auto-action.
//!
//! [`GlobalLockBadge`] owns the single 1 Hz poll loop (modelled on
//! `resolume_health.rs`: `try_get_untracked()` / `try_set()`, break on
//! disposal — the `sp-ui-frontend.md` rule) that fills `store.ndi_health`;
//! every per-card [`LockBadge`] reads that same store signal.

use leptos::prelude::*;
use sp_core::genlock::lock_state::LockState;

use crate::api::NdiOutputHealth;
use crate::store::DashboardStore;

/// #164: a per-card lock badge is shown only when pacing is enabled AND the
/// output is on the wall (Playing or Paused). While `genlock_pacing` is OFF —
/// the production default (#147) — no badge is shown at all, so the dashboard
/// is not littered with '● UNLOCKED — pacing disabled' on every card.
pub fn should_show_lock_badge(o: &NdiOutputHealth) -> bool {
    o.pacing.enabled && matches!(o.state.as_str(), "Playing" | "Paused")
}

/// Rank an output's effective lock state for "which is worst" (UNLOCKED worst).
fn severity(state: LockState) -> u8 {
    match state {
        LockState::Locked => 0,
        LockState::Degraded => 1,
        LockState::Unlocked => 2,
    }
}

/// An output's effective lock state: a LOCKED output whose clock is not ok is
/// demoted to UNLOCKED (mirrors `sp_core::genlock::lock_state::summarize`).
fn effective_state(o: &NdiOutputHealth) -> LockState {
    if o.lock_state == LockState::Locked && !o.clock.clock_ok {
        LockState::Unlocked
    } else {
        o.lock_state
    }
}

/// Full CSS class list for a badge in `state` (`lock-badge lock-locked` …).
fn badge_class(state: LockState) -> String {
    let modifier = match state {
        LockState::Locked => "lock-locked",
        LockState::Degraded => "lock-degraded",
        LockState::Unlocked => "lock-unlocked",
    };
    format!("lock-badge {modifier}")
}

/// Per-output badge text: `● LOCKED` / `● DEGRADED <reason>` /
/// `● UNLOCKED <reason>`.
fn badge_text(o: &NdiOutputHealth) -> String {
    match o.lock_state {
        LockState::Locked => "● LOCKED".to_string(),
        LockState::Degraded => format!("● DEGRADED {}", o.lock_reason),
        LockState::Unlocked => format!("● UNLOCKED {}", o.lock_reason),
    }
}

/// Tooltip: clock / pacing / audio / receiver values for one output.
fn badge_title(o: &NdiOutputHealth) -> String {
    let offset = o
        .clock
        .offset_ns
        .map(|n| format!("{n}ns"))
        .unwrap_or_else(|| "—".to_string());
    let mode = if o.clock.mode.is_empty() {
        "—"
    } else {
        o.clock.mode.as_str()
    };
    format!(
        "clock: locked={} mode={} offset={} | pacing: late={} p99={}us repeats={} resyncs={} lag={} | audio: {:+.1}ppm underruns={} | receiver: connections={}",
        o.clock.is_locked,
        mode,
        offset,
        o.pacing.late_frames,
        o.pacing.jitter_p99_us,
        o.pacing.repeats,
        o.pacing.resyncs,
        o.pacing.lag_slots,
        o.audio.residual_ppm,
        o.audio.underruns,
        o.connections,
    )
}

/// #164: the dashboard header's whole-box genlock summary — `Some((class,
/// text))`, or `None` when NO output has pacing enabled (the production
/// default), so the header shows nothing at all rather than a spurious
/// '● UNLOCKED — pacing disabled'.
///
/// Computed over the pacing-ENABLED outputs; the summary reflects the LIVE
/// (Playing) ones. Keeps the LOCKED / DEGRADED / UNLOCKED vocabulary and adds
/// an `n/m` locked count plus the worst output's reason when there is a fault.
fn global_summary(health: &[NdiOutputHealth]) -> Option<(String, String)> {
    let enabled: Vec<&NdiOutputHealth> = health.iter().filter(|o| o.pacing.enabled).collect();
    if enabled.is_empty() {
        return None;
    }

    let live: Vec<&NdiOutputHealth> = enabled.iter().copied().filter(|o| o.is_live()).collect();
    let m = live.len();
    if m == 0 {
        // Pacing enabled but nothing on the wall — the state comes from the
        // clock only (LOCKED iff every output's clock is ok, else UNLOCKED),
        // matching `sp_core::genlock::lock_state::summarize` and the post-deploy
        // consistency oracle.
        let clock_ok = enabled.iter().all(|o| o.clock.clock_ok);
        let state = if clock_ok {
            LockState::Locked
        } else {
            LockState::Unlocked
        };
        let text = if clock_ok {
            "● LOCKED (no live output)".to_string()
        } else {
            "● UNLOCKED (no live output)".to_string()
        };
        return Some((badge_class(state), text));
    }

    let worst_state = live
        .iter()
        .copied()
        .map(effective_state)
        .max_by_key(|s| severity(*s))
        .expect("m > 0 guarantees a live output");
    let worst = live
        .iter()
        .copied()
        .find(|&o| effective_state(o) == worst_state)
        .expect("worst_state was derived from a live output");
    let locked = live
        .iter()
        .copied()
        .filter(|&o| effective_state(o) == LockState::Locked)
        .count();

    let class = badge_class(worst_state);
    let text = match worst_state {
        LockState::Locked => format!("● LOCKED {locked}/{m}"),
        LockState::Degraded => format!("● DEGRADED {locked}/{m} — {}", worst.lock_reason),
        LockState::Unlocked => format!("● UNLOCKED {locked}/{m} — {}", worst.lock_reason),
    };
    Some((class, text))
}

/// One output's lock badge, coloured + tooltipped. Rendered next to a playlist
/// card's NDI output name.
#[component]
pub fn LockBadge(output: NdiOutputHealth) -> impl IntoView {
    let class = badge_class(output.lock_state);
    let text = badge_text(&output);
    let title = badge_title(&output);
    view! {
        <span class={class} title={title}>{text}</span>
    }
}

/// The dashboard header's whole-box genlock summary badge. Owns the single
/// 1 Hz poll loop that fills `store.ndi_health`; the per-card badges read the
/// same signal.
#[component]
pub fn GlobalLockBadge() -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");

    // Cancellation flag flipped on unmount so the spawn_local loop exits
    // instead of running for the lifetime of the wasm runtime.
    let cancelled = RwSignal::new(false);
    on_cleanup(move || cancelled.set(true));

    let _poll = Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            loop {
                // `cancelled` is page-owned: navigating away disposes it while
                // this task is parked in the 1 s timer, so the next wake must
                // use `try_get_untracked` (None on a disposed signal) and stop
                // rather than panic. `store.ndi_health` is app-root-owned, but
                // `try_set` is safe either way.
                if cancelled.try_get_untracked() != Some(false) {
                    break;
                }
                if let Ok(data) = crate::api::get_ndi_health().await
                    && store.ndi_health.try_set(data).is_some()
                {
                    break;
                }
                gloo_timers::future::TimeoutFuture::new(1_000).await;
            }
        });
    });

    view! {
        <div class="genlock-status">
            {move || {
                let health = store.ndi_health.get();
                // #164: `None` while pacing is disabled everywhere → the header
                // renders no badge at all.
                global_summary(&health)
                    .map(|(class, text)| view! { <span class={class}>{text}</span> })
            }}
        </div>
    }
}
