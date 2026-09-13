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
use sp_core::genlock::lock_state::{LockState, OutputLock, summarize};

use crate::api::NdiOutputHealth;
use crate::store::DashboardStore;

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

/// Class + text for the header's global summary badge from the per-output
/// health, via `sp_core`'s [`summarize`].
fn global_badge(health: &[NdiOutputHealth]) -> (String, String) {
    let outputs: Vec<OutputLock> = health
        .iter()
        .map(|o| OutputLock {
            name: o.ndi_name.clone(),
            state: o.lock_state,
            live: o.is_live(),
            clock_ok: o.clock.clock_ok,
        })
        .collect();
    let summary = summarize(&outputs);
    let class = badge_class(summary.state);
    let text = match summary.state {
        LockState::Locked => {
            if summary.live_count == 0 {
                "● LOCKED (no live output)".to_string()
            } else {
                "● LOCKED".to_string()
            }
        }
        LockState::Degraded => match &summary.worst {
            Some(w) => format!("● DEGRADED — {w}"),
            None => "● DEGRADED".to_string(),
        },
        LockState::Unlocked => match &summary.worst {
            Some(w) => format!("● UNLOCKED — {w}"),
            None if summary.live_count == 0 => "● UNLOCKED (no live output)".to_string(),
            None => "● UNLOCKED".to_string(),
        },
    };
    (class, text)
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
                let (class, text) = global_badge(&health);
                view! { <span class={class}>{text}</span> }
            }}
        </div>
    }
}
