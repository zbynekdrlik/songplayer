//! The Dante row of Nastavenia "Zvukové výstupy" (#285).

use leptos::prelude::*;
use sp_core::asio_resampling::{Chip, asio_resampling_chips};
use sp_core::audio_outputs::OutputEntry;
use sp_core::dante_status::dante_resampling_chips;

use super::{ProgramOutputs, edit, read, running_asio};

/// A running ASIO or Dante output's resampling line, empty otherwise.
pub(super) fn resampling_line(live: &ProgramOutputs, id: &str) -> Vec<Chip> {
    let dante = live.outputs.iter().any(|o| o.id == id && o.kind == "dante");
    match running_asio(live, id) {
        Some(f) if dante => dante_resampling_chips(&f),
        Some(f) => asio_resampling_chips(&f),
        None => Vec::new(),
    }
}

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
