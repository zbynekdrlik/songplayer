//! #233: Nastavenia "Zvukové výstupy" — the program's audio outputs (ONE
//! list, `audio_outputs`) and the network sample rate. It edits
//! `sp_core::audio_outputs` entries, refuses a bad list with the SERVER's
//! own rules (the Slovak text of the first problem) before sending, saves
//! ONLY `audio_outputs` + `audio_network_rate` (its own PATCH — never the
//! other settings, which the form above saves), merges what it saved into
//! `store.settings`, and shows each output's live state (`GET
//! /api/v1/program` → `outputs[]`, every 2 s). Rows are keyed by id and every
//! cell reads the list by id: a refresh never leaves a stale row, typing never
//! drops focus (`sp-ui-frontend.md`). The section re-reads only ITS two
//! settings (a `Memo`), so a save of the form above never resets an unsaved
//! row. Every `<option>` carries `selected`: tachys sets a reactive
//! `prop:value` before a select's options are mounted, which would show the
//! first option after a load. Nothing is saved until the page has LOADED
//! the settings (`loaded`, set by the Settings page): an empty list before
//! the load, or after a failed one, would replace the stored list. A saved
//! entry the server does not run reads "uložený, nespustený" with the
//! server's `outputs_problems` line as its tooltip.
//!
//! Testids (set here): `settings-audio-outputs` (the fieldset),
//! `settings-audio-network-rate`, `audio-outputs-add-vban`,
//! `audio-outputs-save`, `audio-outputs-message` (class
//! `audio-outputs-status`: `.save-status` is the form's alone, which the
//! other Nastavenia specs read unscoped), `audio-outputs-load-error`,
//! and per row `audio-output-row` (`data-id` = the entry's id),
//! `audio-output-name`, `audio-output-enabled`, `audio-output-rate`
//! (`network` or a rate), `audio-output-delay`, `audio-output-vban-host`,
//! `audio-output-vban-port`, `audio-output-vban-stream`,
//! `audio-output-vban-format`, `audio-output-state`, `audio-output-remove`.

use std::collections::HashMap;

use leptos::prelude::*;
use serde::Deserialize;
use sp_core::audio_outputs::{
    OutputEntry, RateChoice, SUPPORTED_RATES, VbanDest, VbanSampleFormat, new_vban, validate_list,
};
use sp_core::config::{SETTING_AUDIO_NETWORK_RATE, SETTING_AUDIO_OUTPUTS, audio_network_rate};

use crate::api;
use crate::store::DashboardStore;

/// One output's live state (the fields this section shows).
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct OutputLive {
    pub id: String,
    pub state: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub latency_ms: f64,
}

/// `GET /api/v1/program`, the outputs only.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct ProgramOutputs {
    #[serde(default)]
    pub outputs: Vec<OutputLive>,
    /// The stored entries the server does not run, each naming its id.
    #[serde(default)]
    pub outputs_problems: Vec<String>,
}

/// Why nothing can be saved: the settings did not load (`Some(false)`).
const NOT_LOADED: &str = "Nastavenia sa nenačítali — výstupy sa nedajú uložiť";

/// The Slovak label of an output's state.
pub fn state_sk(state: &str) -> &'static str {
    match state {
        "running" => "beží",
        "opening" => "otvára sa",
        "waiting" => "čaká",
        "disabled" => "vypnutý",
        _ => "neznámy stav",
    }
}

/// The stored list (`audio_outputs`) the section starts from; `Err` when it
/// cannot be read (the section then refuses to save over it).
pub fn stored_list(stored: Option<&str>) -> Result<Vec<OutputEntry>, String> {
    match stored.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(Vec::new()),
        Some(raw) => serde_json::from_str(raw)
            .map_err(|_| "Uložený zoznam výstupov sa nedá načítať — neukladajte ho".to_string()),
    }
}

/// A rate as the select's value.
pub fn rate_value(rate: RateChoice) -> String {
    match rate {
        RateChoice::Network => "network".to_string(),
        RateChoice::Fixed(hz) => hz.to_string(),
    }
}

/// The select's value as a rate.
pub fn rate_choice(value: &str) -> RateChoice {
    value
        .parse()
        .map(RateChoice::Fixed)
        .unwrap_or(RateChoice::Network)
}

/// A row's state: the server's, else "uložený, nespustený" for a saved entry
/// it does not run (skipped, or not applied yet), else "neuložený".
fn live_text(live: &ProgramOutputs, id: &str, saved: bool) -> String {
    match live.outputs.iter().find(|o| o.id == id) {
        None if saved => "uložený, nespustený".to_string(),
        None => "neuložený".to_string(),
        Some(o) if o.state == "running" => {
            format!("{} · {:.0} ms", state_sk(&o.state), o.latency_ms)
        }
        Some(o) => state_sk(&o.state).to_string(),
    }
}

/// A row's tooltip: the server's reason, or its problem with this entry, in
/// the server's own (English) words, marked as the server's.
fn live_reason(live: &ProgramOutputs, id: &str) -> String {
    let named = format!("(id {id})");
    live.outputs
        .iter()
        .find(|o| o.id == id)
        .and_then(|o| o.reason.clone())
        .or_else(|| {
            live.outputs_problems
                .iter()
                .find(|p| p.contains(&named))
                .cloned()
        })
        .map(|text| format!("Hlásenie servera: {text}"))
        .unwrap_or_default()
}

/// Read one field of the entry `id` (reactive).
fn read<T: Default>(
    list: RwSignal<Vec<OutputEntry>>,
    id: &str,
    f: impl Fn(&OutputEntry) -> T,
) -> T {
    list.with(|l| l.iter().find(|e| e.id == id).map(&f).unwrap_or_default())
}

/// Change the entry `id` in place.
fn edit(list: RwSignal<Vec<OutputEntry>>, id: &str, f: impl FnOnce(&mut OutputEntry)) {
    list.update(|l| {
        if let Some(e) = l.iter_mut().find(|e| e.id == id) {
            f(e);
        }
    });
}

/// `loaded`: the Settings page's load of `GET /api/v1/settings` — `None`
/// while it runs, `Some(false)` when it failed.
#[component]
pub fn AudioOutputs(loaded: RwSignal<Option<bool>>) -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");
    let entries = RwSignal::new(Vec::<OutputEntry>::new());
    let network_rate = RwSignal::new(audio_network_rate(None).to_string());
    let load_error = RwSignal::new(None::<String>);
    let message = RwSignal::new(String::new());
    let live = RwSignal::new(ProgramOutputs::default());

    // Only the section's own two settings: the form above merges its save
    // into `store.settings`, which must not reset an unsaved row here.
    let stored = Memo::new(move |_| {
        store.settings.with(|s| {
            (
                s.get(SETTING_AUDIO_OUTPUTS).cloned(),
                s.get(SETTING_AUDIO_NETWORK_RATE).cloned(),
            )
        })
    });
    let saved_ids = Memo::new(move |_| {
        stored.with(|(list, _)| {
            stored_list(list.as_deref())
                .map(|l| l.into_iter().map(|e| e.id).collect::<Vec<_>>())
                .unwrap_or_default()
        })
    });
    let blocker = move || match loaded.get() {
        Some(true) => load_error.get(),
        Some(false) => Some(NOT_LOADED.to_string()),
        None => None,
    };
    let _sync = Effect::new(move |_| {
        let (list, rate) = stored.get();
        match stored_list(list.as_deref()) {
            Ok(list) => {
                entries.set(list);
                load_error.set(None);
            }
            Err(e) => {
                entries.set(Vec::new());
                load_error.set(Some(e));
            }
        }
        network_rate.set(audio_network_rate(rate.as_deref()).to_string());
    });

    let cancelled = RwSignal::new(false);
    on_cleanup(move || cancelled.set(true));
    let _poll = Effect::new(move |_| {
        crate::store::poll_into("/api/v1/program", 2_000, cancelled, live);
    });

    // A new id above every row AND every stored entry: a row removed but not
    // saved yet still runs under its id, and a new row must never take it.
    let add_vban = move |_: leptos::ev::MouseEvent| {
        let saved =
            stored.with_untracked(|(list, _)| stored_list(list.as_deref()).unwrap_or_default());
        entries.update(|l| {
            let known: Vec<OutputEntry> = l.iter().cloned().chain(saved).collect();
            let fresh = new_vban(&known);
            l.push(fresh);
        });
    };

    let on_save = move |_: leptos::ev::MouseEvent| {
        if loaded.get_untracked() != Some(true) {
            return;
        }
        let list = entries.get();
        if let Err(e) = validate_list(&list) {
            message.set(e.sk());
            return;
        }
        let Ok(text) = serde_json::to_string(&list) else {
            return;
        };
        let mut body = HashMap::new();
        body.insert(SETTING_AUDIO_OUTPUTS.to_string(), text);
        body.insert(SETTING_AUDIO_NETWORK_RATE.to_string(), network_rate.get());
        leptos::task::spawn_local(async move {
            message.set("Ukladám…".into());
            match api::patch_json_empty("/api/v1/settings", &body).await {
                Ok(()) => {
                    message.set("Uložené".into());
                    store.settings.update(move |s| s.extend(body));
                }
                Err(_) => message.set("Chyba pri ukladaní".into()),
            }
        });
    };

    view! {
        <fieldset class="audio-outputs" data-testid="settings-audio-outputs">
            <legend>"Zvukové výstupy"</legend>
            <label>
                "Vzorkovacia frekvencia siete"
                <select
                    data-testid="settings-audio-network-rate"
                    prop:value=move || network_rate.get()
                    on:change=move |ev| network_rate.set(event_target_value(&ev))
                >
                    {SUPPORTED_RATES
                        .iter()
                        .map(|&r| {
                            view! {
                                <option
                                    value=r.to_string()
                                    selected=move || network_rate.get() == r.to_string()
                                >
                                    {format!("{r} Hz")}
                                </option>
                            }
                        })
                        .collect_view()}
                </select>
            </label>
            {move || {
                blocker()
                    .map(|e| {
                        view! {
                            <p class="audio-outputs-error" data-testid="audio-outputs-load-error">
                                {e}
                            </p>
                        }
                    })
            }}
            <For
                each=move || entries.with(|l| l.iter().map(|e| e.id.clone()).collect::<Vec<_>>())
                key=|id| id.clone()
                children=move |id| {
                    view! { <OutputRow id=id entries=entries live=live saved_ids=saved_ids /> }
                }
            />
            <div class="form-actions">
                <button type="button" data-testid="audio-outputs-add-vban" on:click=add_vban>
                    "Pridať výstup VBAN"
                </button>
                <button
                    type="button"
                    data-testid="audio-outputs-save"
                    prop:disabled=move || loaded.get() != Some(true) || load_error.get().is_some()
                    on:click=on_save
                >
                    "Uložiť výstupy"
                </button>
                <span class="audio-outputs-status" data-testid="audio-outputs-message">
                    {move || message.get()}
                </span>
            </div>
        </fieldset>
    }
}

#[component]
fn OutputRow(
    id: String,
    entries: RwSignal<Vec<OutputEntry>>,
    live: RwSignal<ProgramOutputs>,
    saved_ids: Memo<Vec<String>>,
) -> impl IntoView {
    let row_id = id.clone();
    let id = StoredValue::new(id);
    let vban = move |f: fn(&VbanDest) -> String| {
        read(entries, &id.get_value(), move |e| {
            e.vban.as_ref().map(f).unwrap_or_default()
        })
    };
    let rate_is = move |r: RateChoice| read(entries, &id.get_value(), |e| e.rate) == r;
    let format_is =
        move |f: VbanSampleFormat| vban(|v| v.format.as_str().to_string()) == f.as_str();
    view! {
        <div class="audio-output-row" data-testid="audio-output-row" data-id=row_id>
            <label>
                "Názov"
                <input
                    type="text"
                    data-testid="audio-output-name"
                    prop:value=move || read(entries, &id.get_value(), |e| e.name.clone())
                    on:input=move |ev| {
                        let v = event_target_value(&ev);
                        edit(entries, &id.get_value(), |e| e.name = v);
                    }
                />
            </label>
            <label>
                <input
                    type="checkbox"
                    data-testid="audio-output-enabled"
                    prop:checked=move || read(entries, &id.get_value(), |e| e.enabled)
                    on:change=move |ev| {
                        let v = event_target_checked(&ev);
                        edit(entries, &id.get_value(), |e| e.enabled = v);
                    }
                />
                "Zapnutý"
            </label>
            <label>
                "Frekvencia"
                <select
                    data-testid="audio-output-rate"
                    prop:value=move || rate_value(read(entries, &id.get_value(), |e| e.rate))
                    on:change=move |ev| {
                        let v = rate_choice(&event_target_value(&ev));
                        edit(entries, &id.get_value(), |e| e.rate = v);
                    }
                >
                    <option value="network" selected=move || rate_is(RateChoice::Network)>
                        "podľa siete"
                    </option>
                    {SUPPORTED_RATES
                        .iter()
                        .map(|&r| {
                            view! {
                                <option
                                    value=r.to_string()
                                    selected=move || rate_is(RateChoice::Fixed(r))
                                >
                                    {format!("{r} Hz")}
                                </option>
                            }
                        })
                        .collect_view()}
                </select>
            </label>
            <label>
                "Oneskorenie (ms)"
                <input
                    type="number"
                    min="0"
                    max="2000"
                    data-testid="audio-output-delay"
                    prop:value=move || read(entries, &id.get_value(), |e| e.delay_ms.to_string())
                    on:input=move |ev| {
                        let v = event_target_value(&ev).trim().parse().unwrap_or(0);
                        edit(entries, &id.get_value(), |e| e.delay_ms = v);
                    }
                />
            </label>
            <label>
                "Cieľ (host)"
                <input
                    type="text"
                    data-testid="audio-output-vban-host"
                    placeholder="dev1.lan"
                    prop:value=move || vban(|v| v.host.clone())
                    on:input=move |ev| {
                        let v = event_target_value(&ev).trim().to_string();
                        edit(entries, &id.get_value(), |e| {
                            if let Some(d) = e.vban.as_mut() {
                                d.host = v;
                            }
                        });
                    }
                />
            </label>
            <label>
                "Port"
                <input
                    type="number"
                    min="1"
                    max="65535"
                    data-testid="audio-output-vban-port"
                    prop:value=move || vban(|v| v.port.to_string())
                    on:input=move |ev| {
                        let v = event_target_value(&ev).trim().parse().unwrap_or(0);
                        edit(entries, &id.get_value(), |e| {
                            if let Some(d) = e.vban.as_mut() {
                                d.port = v;
                            }
                        });
                    }
                />
            </label>
            <label>
                "Názov streamu"
                <input
                    type="text"
                    maxlength="16"
                    data-testid="audio-output-vban-stream"
                    prop:value=move || vban(|v| v.stream_name.clone())
                    on:input=move |ev| {
                        let v = event_target_value(&ev);
                        edit(entries, &id.get_value(), |e| {
                            if let Some(d) = e.vban.as_mut() {
                                d.stream_name = v;
                            }
                        });
                    }
                />
            </label>
            <label>
                "Formát"
                <select
                    data-testid="audio-output-vban-format"
                    prop:value=move || vban(|v| v.format.as_str().to_string())
                    on:change=move |ev| {
                        let v = VbanSampleFormat::parse(&event_target_value(&ev))
                            .unwrap_or_default();
                        edit(entries, &id.get_value(), |e| {
                            if let Some(d) = e.vban.as_mut() {
                                d.format = v;
                            }
                        });
                    }
                >
                    <option value="int16" selected=move || format_is(VbanSampleFormat::Int16)>
                        "16 bitov"
                    </option>
                    <option value="int24" selected=move || format_is(VbanSampleFormat::Int24)>
                        "24 bitov"
                    </option>
                    <option
                        value="float32"
                        selected=move || format_is(VbanSampleFormat::Float32)
                    >
                        "32 bitov (float)"
                    </option>
                </select>
            </label>
            <span
                class="audio-output-state"
                data-testid="audio-output-state"
                title=move || live_reason(&live.get(), &id.get_value())
            >
                {move || {
                    let id = id.get_value();
                    let saved = saved_ids.with(|ids| ids.contains(&id));
                    live_text(&live.get(), &id, saved)
                }}
            </span>
            <button
                type="button"
                data-testid="audio-output-remove"
                on:click=move |_| entries.update(|l| l.retain(|e| e.id != id.get_value()))
            >
                "Odobrať"
            </button>
        </div>
    }
}
