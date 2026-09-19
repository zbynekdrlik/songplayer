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
use sp_core::genlock::lock_state::{GlobalLock, GlobalLockInput, LockState, global_lock_summary};

use crate::api::NdiOutputHealth;
use crate::store::DashboardStore;

/// #164: a per-card lock badge is shown only when pacing is enabled AND the
/// output is on the wall (Playing or Paused). While `genlock_pacing` is OFF —
/// the production default (#147) — no badge is shown at all, so the dashboard
/// is not littered with '● UNLOCKED — pacing disabled' on every card.
pub fn should_show_lock_badge(o: &NdiOutputHealth) -> bool {
    o.pacing.enabled && matches!(o.state.as_str(), "Playing" | "Paused")
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
    // #192: on the SDK-clocked path the wall-clock audio emitter carries the
    // audio telemetry (silence/ring/jitter); the paced path shows ppm/underruns.
    let audio = if o.audio.emitter.enabled {
        format!(
            "emitter[sdk-video/wallclock-audio] silence={} ring={}ms jitter_p99={}us late={}",
            o.audio.emitter.silence_blocks,
            o.audio.emitter.ring_depth_ms,
            o.audio.emitter.emit_jitter_p99_us,
            o.audio.emitter.late_blocks,
        )
    } else {
        format!(
            "{:+.1}ppm underruns={}",
            o.audio.residual_ppm, o.audio.underruns,
        )
    };
    format!(
        "clock: locked={} mode={} offset={} | pacing: late={} p99={}us repeats={} resyncs={} lag={} | audio: {} | receiver: connections={}",
        o.clock.is_locked,
        mode,
        offset,
        o.pacing.late_frames,
        o.pacing.jitter_p99_us,
        o.pacing.repeats,
        o.pacing.resyncs,
        o.pacing.lag_slots,
        audio,
        o.connections,
    )
}

/// Build the pure `sp_core` inputs for [`global_lock_summary`] from the health
/// snapshot — the ONE place the whole-box genlock state is decided, unit-tested
/// in sp-core (sp-ui has no test job).
fn global_inputs(health: &[NdiOutputHealth]) -> Vec<GlobalLockInput> {
    health
        .iter()
        .map(|o| GlobalLockInput {
            pacing_enabled: o.pacing.enabled,
            live: o.is_live(),
            state: o.lock_state,
            clock_ok: o.clock.clock_ok,
            reason: o.lock_reason.clone(),
        })
        .collect()
}

/// The header genlock badge's tooltip: the OFF explainer (when off) plus one
/// line per output (`name: reason clock_ok=… resyncs=… late=… pacing=…`).
fn global_title(health: &[NdiOutputHealth], off: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    if off {
        parts.push("pacing vypnuté → NDI SDK clock, free-running".to_string());
    }
    for o in health {
        parts.push(format!(
            "{}: {} clock_ok={} resyncs={} late={} pacing={}",
            o.ndi_name,
            o.lock_reason,
            o.clock.clock_ok,
            o.pacing.resyncs,
            o.pacing.late_frames,
            o.pacing.enabled,
        ));
    }
    parts.join(" | ")
}

/// #176: the dashboard header's whole-box genlock summary — ALWAYS rendered
/// (`(class, text, title)`), never hidden. Delegates the state decision to the
/// unit-tested pure `sp_core::genlock::lock_state::global_lock_summary`, then
/// composes the display text: the explicit `● GENLOCK OFF` (grey) when no output
/// has pacing enabled — the production default (#164's hide-when-off rule is
/// revised) — else `● LOCKED / DEGRADED / UNLOCKED` with an `n/m` live-locked
/// count and the worst output's reason.
fn global_summary(health: &[NdiOutputHealth]) -> (String, String, String) {
    let summary = global_lock_summary(&global_inputs(health));
    let off = summary.state == GlobalLock::Off;
    let title = global_title(health, off);

    // Count over the LIVE pacing-enabled outputs (presentation only; the state
    // itself already came from sp-core).
    let live: Vec<&NdiOutputHealth> = health
        .iter()
        .filter(|o| o.pacing.enabled && o.is_live())
        .collect();
    let m = live.len();
    let locked = live
        .iter()
        .copied()
        .filter(|&o| effective_state(o) == LockState::Locked)
        .count();

    let (class, text) = match summary.state {
        GlobalLock::Off => (
            "lock-badge lock-off".to_string(),
            "● GENLOCK OFF".to_string(),
        ),
        // Pacing enabled but nothing live — the state came from the clock only.
        GlobalLock::Locked if m == 0 => (
            badge_class(LockState::Locked),
            "● LOCKED (no live output)".to_string(),
        ),
        GlobalLock::Unlocked if m == 0 => (
            badge_class(LockState::Unlocked),
            "● UNLOCKED (no live output)".to_string(),
        ),
        GlobalLock::Locked => (
            badge_class(LockState::Locked),
            format!("● LOCKED {locked}/{m}"),
        ),
        GlobalLock::Degraded => (
            badge_class(LockState::Degraded),
            format!("● DEGRADED {locked}/{m} — {}", summary.reason),
        ),
        GlobalLock::Unlocked => (
            badge_class(LockState::Unlocked),
            format!("● UNLOCKED {locked}/{m} — {}", summary.reason),
        ),
    };
    (class, text, title)
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
                // #176: ALWAYS render the whole-box badge — grey `● GENLOCK OFF`
                // in the production (pacing-off) default, else LOCKED/DEGRADED/
                // UNLOCKED. Never hidden (revises #164).
                let (class, text, title) = global_summary(&health);
                view! {
                    <span class={class} title={title} data-testid="genlock-global-badge">
                        {text}
                    </span>
                }
            }}
        </div>
    }
}
