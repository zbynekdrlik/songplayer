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

    let read_gain = move |ev: &leptos::ev::Event| -> f32 {
        (event_target_value(ev).parse::<f32>().unwrap_or(0.0) / 100.0).clamp(0.0, 1.0)
    };
    let on_input = move |ev: leptos::ev::Event| {
        gain.set(read_gain(&ev));
    };
    let on_change_ev = move |ev: leptos::ev::Event| {
        let v = read_gain(&ev);
        gain.set(v);
        on_change.run(v);
    };

    let aria = label.clone();
    view! {
        <div class="mixer-channel" class:disabled=move || !enabled.get()>
            <span class="mixer-channel-value">{move || format!("{}%", pct())}</span>
            <input
                type="range"
                class="mixer-fader"
                min="0"
                max="100"
                step="1"
                aria-label=aria
                data-testid=testid.unwrap_or_default()
                prop:value=move || pct()
                prop:disabled=move || !enabled.get()
                on:input=on_input
                on:change=on_change_ev
            />
            <span class="mixer-channel-label">{label}</span>
            // Always render the note slot (empty when live) so the channel keeps a
            // stable height whether or not it carries a "pevné" note.
            <span class="mixer-channel-note">{fixed_note.unwrap_or_default()}</span>
        </div>
    }
}
