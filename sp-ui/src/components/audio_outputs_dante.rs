//! The Dante row of Nastavenia "Zvukové výstupy" (#285).

use leptos::prelude::*;
use sp_core::audio_outputs::OutputEntry;

use super::{edit, read};

#[component]
pub(super) fn DanteFields(
    id: StoredValue<String>,
    entries: RwSignal<Vec<OutputEntry>>,
) -> impl IntoView {
    view! {
        <span class="audio-output-asio-rate" data-testid="audio-output-dante-rate">
            "Frekvencia: podľa siete, 2 kanály"
        </span>
        <label>
            "Názov zariadenia"
            <input
                type="text"
                autocomplete="off"
                data-testid="audio-output-dante-device"
                prop:value=move || {
                    read(entries, &id.get_value(), |e| {
                        e.dante.as_ref().map(|d| d.device.clone()).unwrap_or_default()
                    })
                }
                on:input=move |ev| {
                    let v = event_target_value(&ev);
                    edit(entries, &id.get_value(), |e| {
                        if let Some(d) = e.dante.as_mut() {
                            d.device = v;
                        }
                    });
                }
            />
        </label>
        <label>
            "IP adresa tohto PC"
            <input
                type="text"
                autocomplete="off"
                data-testid="audio-output-dante-ip"
                prop:value=move || {
                    read(entries, &id.get_value(), |e| {
                        e.dante.as_ref().map(|d| d.ip.clone()).unwrap_or_default()
                    })
                }
                on:input=move |ev| {
                    let v = event_target_value(&ev);
                    edit(entries, &id.get_value(), |e| {
                        if let Some(d) = e.dante.as_mut() {
                            d.ip = v;
                        }
                    });
                }
            />
        </label>
    }
}
