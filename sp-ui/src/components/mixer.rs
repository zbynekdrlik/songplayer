//! The ONE modern mixer (#181 D2): a channel strip of vertical faders + a preset
//! row, shared by the song (karaoke) and dub adapters. Presentational only — the
//! adapters own the API wiring and pass the channels, presets, title, state line
//! and the disabled reason in. Replaces the old `karaoke_control.rs` visuals; the
//! per-song stems state contract (#177) is preserved by the karaoke adapter.

use leptos::prelude::*;

use crate::components::mixer_channel::{ChannelSpec, MixerChannel};

/// One preset button (e.g. a karaoke mode, or a dub blend point).
#[derive(Clone)]
pub struct PresetSpec {
    /// Stable wire id, matched against `active_preset` for the highlight.
    pub id: String,
    /// Slovak button label.
    pub label: String,
    /// Fired when the operator picks this preset.
    pub on_select: Callback<()>,
}

#[component]
pub fn Mixer(
    /// Widget heading.
    #[prop(into)] title: Signal<String>,
    /// The binding / now-playing state line (e.g. "Stemy — …").
    #[prop(into)] state_line: Signal<String>,
    /// Stable test id for the state line.
    #[prop(optional)] state_testid: &'static str,
    /// Reason the controls are locked; `None`/empty = not locked.
    #[prop(into)] disabled_reason: Signal<Option<String>>,
    /// The fader channels, in order.
    channels: Vec<ChannelSpec>,
    /// The preset buttons.
    presets: Vec<PresetSpec>,
    /// Which preset id is active (highlighted with the accent).
    #[prop(into)] active_preset: Signal<Option<String>>,
    /// Stable test id for the presets group.
    #[prop(optional)] presets_testid: &'static str,
    /// Extra class on the root (e.g. "mixer-karaoke" / "mixer-dub").
    #[prop(optional)] extra_class: &'static str,
    /// Adapter-specific footer content (e.g. the enqueue button + save status).
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let reason_hidden =
        move || disabled_reason.get().map(|r| r.trim().is_empty()).unwrap_or(true);
    // One reactive class string (base + variant + locked) — avoids mixing a
    // dynamic `class=` with `class:` toggles.
    let root_class = move || {
        let mut c = format!("mixer {extra_class}");
        if !reason_hidden() {
            c.push_str(" mixer-locked");
        }
        c
    };

    view! {
        <section class=root_class>
            <h3 class="mixer-title">{move || title.get()}</h3>
            <p class="mixer-state" data-testid=state_testid>
                {move || state_line.get()}
            </p>
            <p class="mixer-reason" class:hidden=reason_hidden>
                {move || disabled_reason.get().unwrap_or_default()}
            </p>

            <div class="mixer-channels">
                {channels
                    .into_iter()
                    .map(|ch| view! { <MixerChannel ch=ch /> })
                    .collect_view()}
            </div>

            <div class="mixer-presets" data-testid=presets_testid role="group">
                {presets
                    .into_iter()
                    .map(|p| {
                        let PresetSpec { id, label, on_select } = p;
                        let id_for_active = id.clone();
                        let is_active = move || {
                            active_preset.get().as_deref() == Some(id_for_active.as_str())
                        };
                        view! {
                            <button
                                type="button"
                                class="mixer-preset"
                                class:active=is_active
                                data-testid=format!("mixer-preset-{id}")
                                on:click=move |_| on_select.run(())
                            >
                                {label}
                            </button>
                        }
                    })
                    .collect_view()}
            </div>

            {children.map(|c| view! { <div class="mixer-footer">{c()}</div> })}
        </section>
    }
}
