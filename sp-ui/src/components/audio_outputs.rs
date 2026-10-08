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
//! #233 release review (the decisions are `sp_core::audio_outputs_save`,
//! unit-tested): right before its PATCH the save re-reads
//! `GET /api/v1/settings` and refuses, in Slovak, a list changed on the
//! server since the load (or a rate, when it would send one) and a save while
//! the old `vban_*` keys still wait for their migration; it sends the rate
//! only when it changed. A delay or channel field that holds no whole number
//! is refused at save (never stored as 0 or ignored), "Uložené" goes away at
//! the next edit, "Odobrať" removes one row, and a waiting VBAN row reads its
//! reason in Slovak (the server's `reason_code`).
//!
//! #233 lane 3: an ASIO output ("Pridať výstup ASIO") picks a driver from
//! `GET /api/v1/audio/asio-drivers` (loaded once; the add button waits for
//! it) — a stored driver the box does not list stays, marked "(nenájdený)" —
//! and its left / right channels, shown 1-based. Its rate is the driver's.
//! Each row shows only its own type's fields (`<Show>` on a `Memo` of the
//! type, so typing never re-creates them). A running ASIO output reads its
//! latency, correction and underruns; a waiting one its reason in Slovak
//! (`sp_core::audio_outputs::asio_reason_sk`) and its next try.
//!
//! Testids (set here): `settings-audio-outputs` (the fieldset),
//! `settings-audio-network-rate`, `audio-outputs-add-vban`,
//! `audio-outputs-add-asio`,
//! `audio-outputs-save`, `audio-outputs-message` (class
//! `audio-outputs-status`: `.save-status` is the form's alone, which the
//! other Nastavenia specs read unscoped), `audio-outputs-load-error`,
//! and per row `audio-output-row` (`data-id` = the entry's id),
//! `audio-output-name`, `audio-output-enabled`, `audio-output-rate`
//! (`network` or a rate), `audio-output-delay`, `audio-output-vban-host`,
//! `audio-output-vban-port`, `audio-output-vban-stream`,
//! `audio-output-vban-format`, `audio-output-type` ("VBAN" / "ASIO"),
//! `audio-output-asio-driver`, `audio-output-asio-left`,
//! `audio-output-asio-right` (1-based), `audio-output-asio-rate`,
//! `audio-output-state`, `audio-output-remove`.

use std::collections::HashMap;

use leptos::prelude::*;
use serde::Deserialize;
use sp_core::asio_resampling::{
    AsioFigures, Chip, LastFault, asio_resampling_chips, asio_state_chips,
};
use sp_core::audio_outputs::{
    OutputEntry, OutputType, RateChoice, SUPPORTED_RATES, VbanDest, VbanSampleFormat,
    asio_add_refusal, asio_channel_index, asio_channel_shown, asio_driver_options,
    asio_waiting_text, new_asio, new_vban, validate_list,
};
use sp_core::audio_outputs_save::{
    NOT_CHECKED, SAVED, first_unreadable, latency_sk, parse_whole, rate_to_send, remove_one,
    save_refusal, shown_message, vban_waiting_text,
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
    /// #233 release review: a waiting VBAN output's reason as a stable code
    /// (`vban_waiting_text` shows it in Slovak).
    #[serde(default)]
    pub reason_code: Option<String>,
    #[serde(default)]
    pub latency_ms: f64,
    /// #233: a running ASIO driver off the network's rate or with a long
    /// buffer (the server's English, shown as the tooltip).
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub asio: Option<AsioLive>,
}

/// An ASIO output's live numbers (`outputs[i].asio`, the fields shown).
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct AsioLive {
    #[serde(default)]
    pub ppm: f64,
    #[serde(default)]
    pub underruns: u64,
    #[serde(default)]
    pub reason_code: Option<String>,
    #[serde(default)]
    pub retry_in_s: Option<f64>,
    /// #233 (the owner's resampling row): the driver's rate, the card's clock
    /// against SongPlayer's and its lock, the offset the ratio drains and
    /// its time left, the hard re-centres and the last one.
    #[serde(default)]
    pub driver_rate: u32,
    #[serde(default)]
    pub rate_ppm: f64,
    #[serde(default)]
    pub locked: bool,
    #[serde(default)]
    pub offset_ms: f64,
    #[serde(default)]
    pub slew_eta_s: Option<f64>,
    /// #233 Q1: the reserve an underrun left, ms.
    #[serde(default)]
    pub cushion_ms: f64,
    #[serde(default)]
    pub hard_recentres: u64,
    #[serde(default)]
    pub last_hard_recentre: Option<HardRecentreLive>,
}

/// `outputs[i].asio.last_hard_recentre`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct HardRecentreLive {
    #[serde(default)]
    pub cause: String,
    #[serde(default)]
    pub ms: f64,
    #[serde(default)]
    pub lateness_ms: f64,
    #[serde(default)]
    pub ago_s: f64,
}

/// A running ASIO output's figures (`sp_core::asio_resampling`).
fn asio_figures(o: &OutputLive, a: &AsioLive) -> AsioFigures {
    AsioFigures {
        latency_ms: o.latency_ms,
        underruns: a.underruns,
        hard_recentres: a.hard_recentres,
        driver_rate: a.driver_rate,
        rate_ppm: a.rate_ppm,
        locked: a.locked,
        ppm: a.ppm,
        offset_ms: a.offset_ms,
        slew_eta_s: a.slew_eta_s,
        cushion_ms: a.cushion_ms,
        last_fault: a.last_hard_recentre.as_ref().map(|h| LastFault {
            cause: h.cause.clone(),
            ms: h.ms,
            lateness_ms: h.lateness_ms,
            ago_s: h.ago_s,
        }),
    }
}

/// The output `id` when it is an ASIO output that runs: its figures.
fn running_asio(live: &ProgramOutputs, id: &str) -> Option<AsioFigures> {
    let o = live.outputs.iter().find(|o| o.id == id)?;
    let a = o.asio.as_ref()?;
    (o.state == "running").then(|| asio_figures(o, a))
}

/// A row's state line as chips: a running ASIO output's figures (each with
/// its tooltip), else the one state text (the row's tooltip shows through).
fn state_chips(live: &ProgramOutputs, id: &str, saved: bool) -> Vec<Chip> {
    match running_asio(live, id) {
        Some(f) => asio_state_chips(state_sk("running"), &f),
        None => vec![Chip {
            key: "audio-output-state-text",
            text: live_text(live, id, saved),
            title: None,
        }],
    }
}

/// Chips separated by " · " (the separators are text too, so the line reads
/// whole).
fn chips_view(chips: Vec<Chip>) -> impl IntoView {
    chips
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            let sep = (i > 0).then(|| view! { <span class="audio-chip-sep">" · "</span> });
            view! {
                {sep}
                <span class="audio-chip" data-testid=c.key title=c.title>
                    {c.text}
                </span>
            }
        })
        .collect_view()
}

/// `GET /api/v1/audio/asio-drivers`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
struct AsioDriverList {
    #[serde(default)]
    drivers: Vec<String>,
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
        Some(o) => output_text(o),
    }
}

/// One output's state line: running = its latency, labelled as an ASIO
/// row's (a running ASIO output is chips instead, `state_chips`); an output
/// that waits = why, in Slovak (an ASIO output also when it tries again).
fn output_text(o: &OutputLive) -> String {
    let state = state_sk(&o.state);
    match (&o.asio, o.state.as_str()) {
        (Some(_), "running") => state.to_string(),
        (None, "running") => format!("{state} · {}", latency_sk(o.latency_ms)),
        (Some(a), _) => asio_waiting_text(state, a.reason_code.as_deref(), a.retry_in_s),
        (None, _) => vban_waiting_text(state, o.reason_code.as_deref()),
    }
}

/// A row's tooltip: the server's reason, or its problem with this entry, in
/// the server's own (English) words, marked as the server's.
fn live_reason(live: &ProgramOutputs, id: &str) -> String {
    let named = format!("(id {id})");
    live.outputs
        .iter()
        .find(|o| o.id == id)
        .and_then(|o| o.reason.clone().or_else(|| o.note.clone()))
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

/// The number fields typed unreadable since their last good value: `(id,
/// field, slot)` — the slot tells the two channels of one field apart.
type Unreadable = RwSignal<Vec<(String, &'static str, u8)>>;

/// Note (`unreadable`) or clear one number field of the row `id`.
fn mark(bad: Unreadable, id: &str, field: &'static str, slot: u8, unreadable: bool) {
    bad.update(|b| {
        b.retain(|(i, f, s)| !(i == id && *f == field && *s == slot));
        if unreadable {
            b.push((id.to_string(), field, slot));
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
    // What the last save sent (the rows, the rate): "Uložené" only while
    // they are what the section holds (`shown_message`).
    let saved = RwSignal::new(None::<(Vec<OutputEntry>, String)>);
    let bad: Unreadable = RwSignal::new(Vec::new());
    let live = RwSignal::new(ProgramOutputs::default());
    // The box's ASIO drivers, once: `None` until they are read, and for
    // good when the read failed (`drivers_failed`) — never an empty list
    // standing in for one the dashboard was not told (#233 review round 1).
    let drivers = RwSignal::new(None::<Vec<String>>);
    let drivers_failed = RwSignal::new(false);
    leptos::task::spawn_local(async move {
        match api::get::<AsioDriverList>("/api/v1/audio/asio-drivers").await {
            Ok(list) => {
                let _ = drivers.try_set(Some(list.drivers));
            }
            Err(_) => {
                let _ = drivers_failed.try_set(true);
            }
        }
    });

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
    // Why "Pridať výstup ASIO" is off (loading, a failed read, no driver).
    let add_refusal =
        Memo::new(move |_| drivers.with(|d| asio_add_refusal(d.as_deref(), drivers_failed.get())));
    // An ASIO output on the first listed driver (only once a driver is
    // listed: `add_refusal`).
    let add_asio = move |_: leptos::ev::MouseEvent| {
        if add_refusal.get_untracked().is_some() {
            return;
        }
        let saved =
            stored.with_untracked(|(list, _)| stored_list(list.as_deref()).unwrap_or_default());
        let first = drivers
            .with_untracked(|d| d.as_ref().and_then(|d| d.first().cloned()))
            .unwrap_or_default();
        entries.update(|l| {
            let known: Vec<OutputEntry> = l.iter().cloned().chain(saved).collect();
            let fresh = new_asio(&known, &first);
            l.push(fresh);
        });
    };

    let on_save = move |_: leptos::ev::MouseEvent| {
        // Never save before the load landed, nor over a stored list this
        // dashboard cannot read (its rows were reset to none).
        if loaded.get_untracked() != Some(true) || load_error.get_untracked().is_some() {
            return;
        }
        let list = entries.get();
        let unreadable = bad.with(|b| {
            let pairs: Vec<(String, &'static str)> = b
                .iter()
                .map(|(id, field, _)| (id.clone(), *field))
                .collect();
            first_unreadable(&list, &pairs)
        });
        if let Some(why) = unreadable {
            message.set(why);
            return;
        }
        if let Err(e) = validate_list(&list) {
            message.set(e.sk());
            return;
        }
        let Ok(text) = serde_json::to_string(&list) else {
            return;
        };
        // What the page loaded (the store after this section's last save).
        let (loaded_list, loaded_rate) = stored.get_untracked();
        let chosen = network_rate.get_untracked();
        let rate = rate_to_send(loaded_rate.as_deref(), &chosen);
        leptos::task::spawn_local(async move {
            message.set("Ukladám…".into());
            // The server NOW: a list or a rate changed elsewhere since the
            // load, or a migration still pending, is never overwritten.
            let Ok(now) = api::get::<HashMap<String, String>>("/api/v1/settings").await else {
                message.set(NOT_CHECKED.into());
                return;
            };
            if let Some(why) = save_refusal(
                loaded_list.as_deref(),
                loaded_rate.as_deref(),
                &now,
                rate.is_some(),
            ) {
                message.set(why.into());
                return;
            }
            let mut body = HashMap::new();
            body.insert(SETTING_AUDIO_OUTPUTS.to_string(), text);
            if let Some(rate) = rate {
                body.insert(SETTING_AUDIO_NETWORK_RATE.to_string(), rate);
            }
            match api::patch_json_empty("/api/v1/settings", &body).await {
                Ok(()) => {
                    saved.set(Some((list, chosen)));
                    message.set(SAVED.into());
                    store.settings.update(move |s| s.extend(body));
                }
                Err(_) => message.set("Chyba pri ukladaní".into()),
            }
        });
    };
    // "Uložené" only while the rows and the rate are what was saved.
    let message_shown = move || {
        let unchanged = saved.with(|s| {
            s.as_ref().is_some_and(|(list, rate)| {
                entries.with(|e| e == list) && network_rate.with(|r| r == rate)
            })
        });
        message.with(|m| shown_message(m, unchanged).to_string())
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
                    view! {
                        <OutputRow
                            id=id
                            entries=entries
                            live=live
                            saved_ids=saved_ids
                            drivers=drivers
                            bad=bad
                        />
                    }
                }
            />
            <div class="form-actions">
                <button type="button" data-testid="audio-outputs-add-vban" on:click=add_vban>
                    "Pridať výstup VBAN"
                </button>
                <button
                    type="button"
                    data-testid="audio-outputs-add-asio"
                    prop:disabled=move || add_refusal.get().is_some()
                    title=move || add_refusal.get().unwrap_or("")
                    on:click=add_asio
                >
                    "Pridať výstup ASIO"
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
                    {message_shown}
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
    drivers: RwSignal<Option<Vec<String>>>,
    bad: Unreadable,
) -> impl IntoView {
    let row_id = id.clone();
    let id = StoredValue::new(id);
    // The row's type: a Memo, so an edit of a field re-creates no field.
    let kind = Memo::new(move |_| read(entries, &id.get_value(), |e| Some(e.kind)));
    view! {
        <div class="audio-output-row" data-testid="audio-output-row" data-id=row_id>
            <span class="audio-output-type" data-testid="audio-output-type">
                {move || if kind.get() == Some(OutputType::Asio) { "ASIO" } else { "VBAN" }}
            </span>
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
                "Oneskorenie (ms)"
                <input
                    type="number"
                    min="0"
                    max="2000"
                    data-testid="audio-output-delay"
                    prop:value=move || read(entries, &id.get_value(), |e| e.delay_ms.to_string())
                    on:input=move |ev| {
                        // A delay that is no whole number is noted, and
                        // refused at save (never stored as 0).
                        let row = id.get_value();
                        let parsed = parse_whole(&event_target_value(&ev));
                        mark(bad, &row, "delay_ms", 0, parsed.is_none());
                        if let Some(v) = parsed {
                            edit(entries, &row, |e| e.delay_ms = v);
                        }
                    }
                />
            </label>
            <Show when=move || kind.get() == Some(OutputType::Asio)>
                <AsioFields id=id entries=entries drivers=drivers bad=bad />
            </Show>
            <Show when=move || kind.get() == Some(OutputType::Vban)>
                <VbanFields id=id entries=entries />
            </Show>
            <LiveState id=id live=live saved_ids=saved_ids />
            <button
                type="button"
                data-testid="audio-output-remove"
                on:click=move |_| {
                    // The id's marks go with its last row: a later row may
                    // take the id again.
                    let row = id.get_value();
                    entries.update(|l| remove_one(l, &row));
                    if entries.with_untracked(|l| l.iter().all(|e| e.id != row)) {
                        bad.update(|b| b.retain(|(i, _, _)| *i != row));
                    }
                }
            >
                "Odobrať"
            </button>
        </div>
    }
}

/// An ASIO row's own fields: the rate (the driver's), the driver (a stored
/// one the box does not list stays, marked once the list is known), the
/// two channels shown 1-based.
#[component]
fn AsioFields(
    id: StoredValue<String>,
    entries: RwSignal<Vec<OutputEntry>>,
    drivers: RwSignal<Option<Vec<String>>>,
    bad: Unreadable,
) -> impl IntoView {
    let asio_driver = Memo::new(move |_| {
        read(entries, &id.get_value(), |e| {
            e.asio
                .as_ref()
                .map(|a| a.driver.clone())
                .unwrap_or_default()
        })
    });
    // The listed drivers, and a stored one the box does not list (kept;
    // marked only once the list is known): (value, label).
    let driver_options = Memo::new(move |_| {
        let current = asio_driver.get();
        drivers.with(|listed| asio_driver_options(listed.as_deref(), &current))
    });
    // An ASIO channel shown 1-based; an entry that is no number edits nothing.
    let channel = move |i: usize| {
        read(entries, &id.get_value(), move |e| {
            e.asio
                .as_ref()
                .map(|a| asio_channel_shown(a.channels[i]).to_string())
                .unwrap_or_default()
        })
    };
    // A channel that is no whole number is noted, and refused at save.
    let set_channel = move |i: usize, text: String| {
        let row = id.get_value();
        let parsed = parse_whole(&text);
        mark(bad, &row, "asio.channels", i as u8, parsed.is_none());
        if let Some(n) = parsed {
            edit(entries, &row, |e| {
                if let Some(a) = e.asio.as_mut() {
                    a.channels[i] = asio_channel_index(n);
                }
            });
        }
    };
    view! {
        <span class="audio-output-asio-rate" data-testid="audio-output-asio-rate">
            "Frekvencia: podľa ovládača"
        </span>
        <label>
            "Ovládač"
            <select
                data-testid="audio-output-asio-driver"
                prop:value=move || asio_driver.get()
                on:change=move |ev| {
                    let v = event_target_value(&ev);
                    edit(entries, &id.get_value(), |e| {
                        if let Some(a) = e.asio.as_mut() {
                            a.driver = v;
                        }
                    });
                }
            >
                {move || {
                    driver_options
                        .get()
                        .into_iter()
                        .map(|(value, label)| {
                            let this = value.clone();
                            view! {
                                <option
                                    value=value
                                    selected=move || asio_driver.get() == this
                                >
                                    {label}
                                </option>
                            }
                        })
                        .collect_view()
                }}
            </select>
        </label>
        <label>
            "Kanál ľavý"
            <input
                type="number"
                min="1"
                max="512"
                data-testid="audio-output-asio-left"
                prop:value=move || channel(0)
                on:input=move |ev| set_channel(0, event_target_value(&ev))
            />
        </label>
        <label>
            "Kanál pravý"
            <input
                type="number"
                min="1"
                max="512"
                data-testid="audio-output-asio-right"
                prop:value=move || channel(1)
                on:input=move |ev| set_channel(1, event_target_value(&ev))
            />
        </label>
    }
}

/// A VBAN row's own fields: the rate, the target, the stream name, the
/// sample format.
#[component]
fn VbanFields(id: StoredValue<String>, entries: RwSignal<Vec<OutputEntry>>) -> impl IntoView {
    let vban = move |f: fn(&VbanDest) -> String| {
        read(entries, &id.get_value(), move |e| {
            e.vban.as_ref().map(f).unwrap_or_default()
        })
    };
    let rate_is = move |r: RateChoice| read(entries, &id.get_value(), |e| e.rate) == r;
    let format_is =
        move |f: VbanSampleFormat| vban(|v| v.format.as_str().to_string()) == f.as_str();
    view! {
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
    }
}

/// A row's live state: the state line (chips for a running ASIO output),
/// the server's reason as its tooltip, and a running ASIO output's
/// resampling line.
#[component]
fn LiveState(
    id: StoredValue<String>,
    live: RwSignal<ProgramOutputs>,
    saved_ids: Memo<Vec<String>>,
) -> impl IntoView {
    view! {
        <span
            class="audio-output-state"
            data-testid="audio-output-state"
            title=move || live_reason(&live.get(), &id.get_value())
        >
            {move || {
                let id = id.get_value();
                let saved = saved_ids.with(|ids| ids.contains(&id));
                chips_view(state_chips(&live.get(), &id, saved))
            }}
        </span>
        // #233 (the owner): a running ASIO output's resampling, its own
        // line (empty otherwise).
        <div class="audio-output-resampling" data-testid="audio-output-resampling">
            {move || {
                let figures = running_asio(&live.get(), &id.get_value());
                chips_view(figures.map(|f| asio_resampling_chips(&f)).unwrap_or_default())
            }}
        </div>
    }
}
