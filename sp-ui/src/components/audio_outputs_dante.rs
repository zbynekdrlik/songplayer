//! The Dante row of Nastavenia "Zvukové výstupy" (#285).

use leptos::prelude::*;
use sp_core::asio_resampling::{AsioFigures, Chip, asio_resampling_chips};
use sp_core::audio_outputs::{OutputEntry, effective_rate};
use sp_core::dante_status::dante_resampling_chips;

use super::{ProgramOutputs, asio_figures, edit, read};

/// The output `id` when it is an ASIO (or Dante) output that runs: its
/// figures.
pub(super) fn running_asio(live: &ProgramOutputs, id: &str) -> Option<AsioFigures> {
    let o = live.outputs.iter().find(|o| o.id == id)?;
    let a = o.asio.as_ref()?;
    (o.state == "running").then(|| asio_figures(o, a))
}

/// A running ASIO or Dante output's resampling line, empty otherwise. A
/// Dante row names the rate its device converts to: the entry's effective
/// rate against the network rate the server applied (#285).
pub(super) fn resampling_line(
    live: &ProgramOutputs,
    entries: RwSignal<Vec<OutputEntry>>,
    id: &str,
) -> Vec<Chip> {
    let dante = live.outputs.iter().any(|o| o.id == id && o.kind == "dante");
    match running_asio(live, id) {
        Some(f) if dante => {
            let rate = read(entries, id, |e| e.rate);
            dante_resampling_chips(&f, effective_rate(rate, live.audio_network_rate))
        }
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
