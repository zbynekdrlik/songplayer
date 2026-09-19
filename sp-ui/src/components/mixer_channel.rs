//! One vertical fader channel for the modern mixer (#181 D2).
//!
//! Presentational: a ≥44px touch target, a live value readout above the fader,
//! keyboard-operable (native `<input type=range>` arrow keys), `input` updates
//! the visible gain and `change` commits it via `on_change`. A read-only channel
//! (e.g. the fixed instrumental / ambient bed) passes `enabled = false` + a
//! `fixed_note` so the UI is honest about what it does not yet control.

use leptos::prelude::*;

/// Data for one mixer channel. Owned (`Clone`) so it embeds cleanly in a list.
#[derive(Clone)]
pub struct ChannelSpec {
    /// Channel name shown under the fader (e.g. "vokál").
    pub label: String,
    /// Live display gain `0.0..=1.0`; the fader writes this on drag.
    pub gain: RwSignal<f32>,
    /// Whether the fader is interactive (reactive — a preset change can flip it).
    pub enabled: Signal<bool>,
    /// A short note for a read-only channel (e.g. "pevné"); `None` for a live one.
    pub fixed_note: Option<String>,
    /// Committed on `change` with the new gain (`0..=1`).
    pub on_change: Callback<f32>,
    /// Optional stable test id for the range input.
    pub testid: Option<String>,
}

#[component]
pub fn MixerChannel(ch: ChannelSpec) -> impl IntoView {
    let ChannelSpec {
        label,
        gain,
        enabled,
        fixed_note,
        on_change,
        testid,
    } = ch;

    let pct = move || (gain.get() * 100.0).round() as i32;

    // #194 hotfix: a drag gate. While the pointer is down the DRAGGED value is
    // authoritative, so a live gain update from the store (an adapter `Effect`,
    // a now-playing re-load) cannot overwrite the fader out from under the
    // finger; exactly ONE commit (`on:change`) fires on release. The pure
    // `fader_display_pct` (unit-tested in sp-core) chooses dragged-vs-live.
    let dragging = RwSignal::new(false);
    let drag_pct = RwSignal::new(0_i32);
    let display_pct =
        move || sp_core::mixer_model::fader_display_pct(dragging.get(), drag_pct.get(), pct());

    let read_pct = move |ev: &leptos::ev::Event| -> i32 {
        event_target_value(ev)
            .parse::<f32>()
            .unwrap_or(0.0)
            .round()
            .clamp(0.0, 100.0) as i32
    };
    let pct_to_gain = |p: i32| -> f32 { (p as f32 / 100.0).clamp(0.0, 1.0) };
    let on_input = move |ev: leptos::ev::Event| {
        // Every input (drag or keyboard) records the pending value in `drag_pct`,
        // which the commit reads on release. During a drag we do NOT touch `gain`
        // (an external sync must not fight the pointer, and the readout follows
        // `drag_pct` via `display_pct`); a keyboard/programmatic change (no
        // pointer session) also updates `gain` so its readout follows live.
        let p = read_pct(&ev);
        drag_pct.set(p);
        if !dragging.get_untracked() {
            gain.set(pct_to_gain(p));
        }
    };
    let on_change_ev = move |_ev: leptos::ev::Event| {
        // Commit the PENDING dragged value, never the DOM value: on a real
        // release the browser fires `pointerup` (which clears `dragging`) BEFORE
        // `change`, so `prop:value` may have already snapped the DOM back to the
        // live gain — reading `drag_pct` commits what the operator actually set.
        let v = pct_to_gain(drag_pct.get_untracked());
        gain.set(v);
        dragging.set(false);
        on_change.run(v);
    };

    let aria = label.clone();
    view! {
        <div class="mixer-channel" class:disabled=move || !enabled.get()>
            <span class="mixer-channel-value">{move || format!("{}%", display_pct())}</span>
            <input
                type="range"
                class="mixer-fader"
                min="0"
                max="100"
                step="1"
                aria-label=aria
                data-testid=testid.unwrap_or_default()
                prop:value=move || display_pct()
                prop:disabled=move || !enabled.get()
                on:pointerdown=move |_| {
                    drag_pct.set(pct());
                    dragging.set(true);
                }
                on:touchstart=move |_| {
                    drag_pct.set(pct());
                    dragging.set(true);
                }
                on:input=on_input
                on:change=on_change_ev
                on:pointerup=move |_| dragging.set(false)
                on:touchend=move |_| dragging.set(false)
                on:pointercancel=move |_| dragging.set(false)
            />
            <span class="mixer-channel-label">{label}</span>
            // Always render the note slot (empty when live) so the channel keeps a
            // stable height whether or not it carries a "pevné" note.
            <span class="mixer-channel-note">{fixed_note.unwrap_or_default()}</span>
        </div>
    }
}
