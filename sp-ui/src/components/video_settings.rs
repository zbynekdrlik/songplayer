//! #223 S13: Nastavenia "Video: sťahovanie a 4K" — the download cap
//! (`max_resolution`), GPU decoding (`video_hw_decode`) and the in-place 4K
//! upgrade (`video_upgrade_enabled`). The settings form owns the three
//! signals and saves them with its own Save, each only when changed
//! (`sp_core::config::*_to_send`). The upgrade's progress
//! (`GET /api/v1/video-upgrade`) is read once when the section shows.

use leptos::prelude::*;
use sp_core::config;
use sp_core::video_upgrade_view::{VideoUpgradeView, cap_label};

use crate::api;

/// The fieldset; `max_resolution` holds the select's value (`""` =
/// automatic), the two flags the checkboxes.
#[component]
pub fn VideoSettings(
    max_resolution: RwSignal<String>,
    hw_decode: RwSignal<bool>,
    upgrade_enabled: RwSignal<bool>,
) -> impl IntoView {
    let status = RwSignal::new(None::<Result<VideoUpgradeView, String>>);
    leptos::task::spawn_local(async move {
        status.set(Some(
            api::get::<VideoUpgradeView>("/api/v1/video-upgrade").await,
        ));
    });
    // The choices, plus a value set through the API that is none of them
    // (shown as itself, never as "automatic"); every option carries
    // `selected`, as the options re-render with the value.
    let options = move || {
        let current = max_resolution.get();
        let mut values: Vec<String> = config::MAX_RESOLUTION_CHOICES
            .iter()
            .map(|choice| (*choice).to_string())
            .collect();
        if !values.contains(&current) {
            values.push(current.clone());
        }
        values
            .into_iter()
            .map(|value| {
                let selected = value == current;
                let label = cap_label(&value);
                view! { <option value=value selected=selected>{label}</option> }
            })
            .collect_view()
    };
    view! {
        <fieldset data-testid="settings-video">
            <legend>"Video: sťahovanie a 4K"</legend>
            <label>
                "Najvyššie rozlíšenie sťahovania"
                <select
                    data-testid="settings-max-resolution"
                    prop:value=move || max_resolution.get()
                    on:change=move |ev| max_resolution.set(event_target_value(&ev))
                >
                    {options}
                </select>
            </label>
            <label>
                <input
                    type="checkbox"
                    data-testid="settings-video-hw-decode"
                    prop:checked=move || hw_decode.get()
                    on:change=move |ev| hw_decode.set(event_target_checked(&ev))
                />
                "Dekódovať video na GPU"
            </label>
            <label title="Stiahnuté piesne sa po jednej nahrádzajú vyšším rozlíšením (4K pri 24/25 snímkach za sekundu), ak ho YouTube má. Pôvodné video ostane, kým sa nová verzia raz neprehrá.">
                <input
                    type="checkbox"
                    data-testid="settings-video-upgrade"
                    prop:checked=move || upgrade_enabled.get()
                    on:change=move |ev| upgrade_enabled.set(event_target_checked(&ev))
                />
                "Vylepšovať stiahnuté videá na vyššie rozlíšenie (4K)"
            </label>
            <span class="settings-hint" data-testid="settings-video-upgrade-status">
                {move || match status.get() {
                    None => "Stav vylepšovania sa načítava…".to_string(),
                    Some(Ok(view)) => view.summary_sk(),
                    Some(Err(_)) => "Stav vylepšovania sa nenačítal".to_string(),
                }}
            </span>
        </fieldset>
    }
}
