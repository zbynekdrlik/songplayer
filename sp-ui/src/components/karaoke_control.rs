//! Karaoke live-control panel (#14): mode dropdown + vocal-gain slider.
//!
//! Drives `GET/POST /api/v1/karaoke`. The mode selects how the separated stems
//! are mixed into the wall's NDI audio; the vocal-gain slider (KaraokeLow only)
//! attenuates the vocals live. Also shows stem-generation progress.

use leptos::prelude::*;

use crate::api;

/// The four karaoke modes as (value, label) pairs. Value matches
/// `sp_core::playback::KaraokeMode::as_str`.
const MODES: &[(&str, &str)] = &[
    ("full_mix", "Plný mix"),
    ("karaoke_low", "Karaoke (stíšené vokály)"),
    ("vocals_only", "Iba vokály"),
    ("instrumental_only", "Iba hudba (karaoke)"),
];

#[component]
pub fn KaraokeControl() -> impl IntoView {
    let mode = RwSignal::new("full_mix".to_string());
    let vocal_gain = RwSignal::new(0.3_f32); // 0.0..=1.0
    let stems_done = RwSignal::new(0_i64);
    let stems_pending = RwSignal::new(0_i64);
    let status = RwSignal::new(String::new());

    // Load the current live state on mount.
    let load = move || {
        leptos::task::spawn_local(async move {
            if let Ok(v) = api::get_karaoke().await {
                if let Some(m) = v.get("mode").and_then(|x| x.as_str()) {
                    mode.set(m.to_string());
                }
                if let Some(g) = v.get("vocal_gain").and_then(|x| x.as_f64()) {
                    vocal_gain.set(g as f32);
                }
                if let Some(d) = v.get("stems_done").and_then(|x| x.as_i64()) {
                    stems_done.set(d);
                }
                if let Some(p) = v.get("stems_pending").and_then(|x| x.as_i64()) {
                    stems_pending.set(p);
                }
            }
        });
    };
    Effect::new(move |_| load());

    // Push the current mode + gain to the server.
    let push = move || {
        let m = mode.get_untracked();
        let g = vocal_gain.get_untracked();
        leptos::task::spawn_local(async move {
            match api::post_karaoke(&m, g).await {
                Ok(()) => status.set("Uložené".into()),
                Err(e) => status.set(format!("Chyba: {e}")),
            }
        });
    };

    let on_mode_change = move |ev: leptos::ev::Event| {
        mode.set(event_target_value(&ev));
        push();
    };

    // Live label on drag; POST on release (`change`).
    let on_gain_input = move |ev: leptos::ev::Event| {
        if let Ok(pct) = event_target_value(&ev).parse::<f32>() {
            vocal_gain.set((pct / 100.0).clamp(0.0, 1.0));
        }
    };
    let on_gain_change = move |_ev: leptos::ev::Event| push();

    view! {
        <div class="karaoke-control">
            <h3>"Karaoke"</h3>
            <label>
                "Režim"
                <select data-testid="karaoke-mode" on:change=on_mode_change>
                    {MODES
                        .iter()
                        .map(|(val, label)| {
                            let val = val.to_string();
                            view! {
                                <option
                                    value=val.clone()
                                    selected=move || mode.get() == val
                                >
                                    {*label}
                                </option>
                            }
                        })
                        .collect_view()}
                </select>
            </label>

            <label class:disabled=move || mode.get() != "karaoke_low">
                "Hlasitosť vokálov: "
                {move || format!("{}%", (vocal_gain.get() * 100.0).round() as i32)}
                <input
                    type="range"
                    min="0"
                    max="100"
                    step="1"
                    data-testid="karaoke-vocal-gain"
                    prop:value=move || (vocal_gain.get() * 100.0).round() as i32
                    prop:disabled=move || mode.get() != "karaoke_low"
                    on:input=on_gain_input
                    on:change=on_gain_change
                />
            </label>

            <p class="karaoke-stem-progress" data-testid="karaoke-stem-progress">
                {move || {
                    let done = stems_done.get();
                    let pending = stems_pending.get();
                    format!("Stemy: {done} hotových, {pending} čaká")
                }}
            </p>
            <span class="save-status">{move || status.get()}</span>
        </div>
    }
}
