//! #278: one band's card under the playlist EQ's curve (iemmixer's band
//! card): its number and type, a log frequency slider, gain, Q — each with
//! its value shown — on / off and delete. A double-click on a value's row
//! resets it to the type's default (`sp_core::audio_fx::default_band`); a
//! type change takes the new type's frequency and Q and keeps the gain
//! (`with_kind`).
//!
//! A slider's input changes the band live (sent throttled); its release
//! (pointer up, or `change` for the keyboard) ends the gesture, sending the
//! last value at once. The card reads its band through a `Memo`, so an
//! edit of another band touches nothing here.

use leptos::prelude::*;
use sp_core::audio_fx::{BandKind, EqBand, GAIN_MAX_DB, GAIN_MIN_DB, default_band, with_kind};
use sp_core::audio_fx_view::{
    SLIDER_STEPS, freq_label, freq_slider, gain_label, q_label, q_slider, slider_freq, slider_q,
    slider_shown, snap_gain,
};

use crate::components::playlist_audio::EqEdit;

/// What a slider row is: its label, test id, tooltip and range.
struct Row {
    label: &'static str,
    testid: &'static str,
    title: Signal<&'static str>,
    min: String,
    max: String,
    step: &'static str,
}

/// One slider row: the label, the slider, its value (`<testid>-value`); a
/// double-click anywhere on the row resets the value. While the pointer
/// drags the thumb, the slider shows the pointer's position (#200's drag
/// gate, `slider_shown`): the band's snapped value maps back to a nearby
/// position, which would make the thumb jump under the pointer.
fn param_row(
    row: Row,
    value: Signal<String>,
    shown: Signal<String>,
    disabled: Signal<bool>,
    slid: impl Fn(String) + 'static,
    reset: impl Fn() + 'static,
    edit: EqEdit,
) -> impl IntoView {
    let title = row.title;
    let dragging = RwSignal::new(false);
    let dragged = RwSignal::new(String::new());
    view! {
        <div class="eq-param" title=move || title.get() on:dblclick=move |_| reset()>
            <span class="eq-param-label">{row.label}</span>
            <input
                type="range"
                data-testid=row.testid
                min=row.min
                max=row.max
                step=row.step
                prop:value=move || slider_shown(dragging.get(), dragged.get(), value.get())
                prop:disabled=move || disabled.get()
                on:pointerdown=move |_| {
                    dragged.set(value.get_untracked());
                    dragging.set(true);
                }
                on:input=move |ev| {
                    let raw = event_target_value(&ev);
                    dragged.set(raw.clone());
                    slid(raw);
                }
                on:change=move |_| {
                    dragging.set(false);
                    edit.finish();
                }
                on:pointerup=move |_| {
                    dragging.set(false);
                    edit.finish();
                }
                on:pointercancel=move |_| dragging.set(false)
            />
            <span class="eq-param-value" data-testid=format!("{}-value", row.testid)>
                {move || shown.get()}
            </span>
        </div>
    }
}

/// Band `id`'s card.
pub(crate) fn band_card(edit: EqEdit, id: u64) -> impl IntoView {
    let band = Memo::new(move |_| edit.band(id));
    let kind = Memo::new(move |_| band.get().map(|b| b.kind));
    let uses_gain = Memo::new(move |_| kind.get().is_some_and(BandKind::uses_gain));
    // A field of the band as text ("" once the band is gone).
    let get = move |read: fn(&EqBand) -> String| band.get().map(|b| read(&b)).unwrap_or_default();

    let freq = param_row(
        Row {
            label: "Frekvencia",
            testid: "band-freq",
            title: Signal::derive(|| "Dvojklik vráti frekvenciu na predvolenú pre tento typ"),
            min: "0".to_string(),
            max: SLIDER_STEPS.to_string(),
            step: "1",
        },
        Signal::derive(move || get(|b| freq_slider(b.freq_hz).to_string())),
        Signal::derive(move || get(|b| freq_label(b.freq_hz))),
        Signal::derive(|| false),
        move |raw| {
            if let Ok(at) = raw.parse::<u32>() {
                edit.edit_band(id, |b| b.freq_hz = slider_freq(at), false);
            }
        },
        move || edit.edit_band(id, |b| b.freq_hz = default_band(b.kind).freq_hz, true),
        edit,
    );
    let gain = param_row(
        Row {
            label: "Zisk",
            testid: "band-gain",
            title: Signal::derive(move || {
                if uses_gain.get() {
                    "Dvojklik vráti zisk na 0 dB"
                } else {
                    "Orezanie basov a výšok zisk nepoužíva"
                }
            }),
            min: GAIN_MIN_DB.to_string(),
            max: GAIN_MAX_DB.to_string(),
            step: "0.1",
        },
        Signal::derive(move || get(|b| format!("{:.1}", b.gain_db))),
        Signal::derive(move || {
            if uses_gain.get() {
                get(|b| gain_label(b.gain_db))
            } else {
                "—".to_string()
            }
        }),
        Signal::derive(move || !uses_gain.get()),
        move |raw| {
            if let Ok(db) = raw.parse::<f64>() {
                edit.edit_band(id, |b| b.gain_db = snap_gain(db), false);
            }
        },
        // A high-/low-pass keeps the gain it does not use (for a later shelf
        // or peak): its greyed row resets nothing.
        move || {
            edit.edit_band(
                id,
                |b| {
                    if b.kind.uses_gain() {
                        b.gain_db = default_band(b.kind).gain_db;
                    }
                },
                true,
            )
        },
        edit,
    );
    let q = param_row(
        Row {
            label: "Q",
            testid: "band-q",
            title: Signal::derive(|| "Dvojklik vráti Q na predvolené pre tento typ"),
            min: "0".to_string(),
            max: SLIDER_STEPS.to_string(),
            step: "1",
        },
        Signal::derive(move || get(|b| q_slider(b.q).to_string())),
        Signal::derive(move || get(|b| q_label(b.q))),
        Signal::derive(|| false),
        move |raw| {
            if let Ok(at) = raw.parse::<u32>() {
                edit.edit_band(id, |b| b.q = slider_q(at), false);
            }
        },
        move || edit.edit_band(id, |b| b.q = default_band(b.kind).q, true),
        edit,
    );

    // Every option carries its own `selected` (the select's value is set
    // before its options mount, sp-ui-frontend.md #233).
    let options = BandKind::ALL
        .into_iter()
        .map(|option| {
            view! {
                <option value={option.name()} selected=move || { kind.get() == Some(option) }>
                    {option.label_sk()}
                </option>
            }
        })
        .collect_view();

    view! {
        <div
            class=move || {
                let (name, on) = band
                    .get()
                    .map(|b| (b.kind.name(), b.enabled))
                    .unwrap_or(("peak", true));
                let off = if on { "" } else { " off" };
                format!("playlist-audio-band kind-{name}{off}")
            }
            data-testid="playlist-audio-band"
            data-index=move || edit.index_of(id)
            data-kind=move || { kind.get().map(BandKind::name) }
            data-freq=move || { band.get().map(|b| b.freq_hz) }
            data-gain=move || { band.get().map(|b| b.gain_db) }
            data-q=move || { band.get().map(|b| b.q) }
            data-enabled=move || { band.get().map(|b| if b.enabled { "true" } else { "false" }) }
        >
            <div class="eq-band-head">
                <span class="band-number" data-testid="band-number">
                    {move || edit.number(id).map(|n| n.to_string()).unwrap_or_default()}
                </span>
                <select
                    data-testid="band-kind"
                    prop:value=move || kind.get().map(BandKind::name).unwrap_or("peak")
                    on:change=move |ev| {
                        if let Some(new_kind) = BandKind::from_name(&event_target_value(&ev)) {
                            edit.edit_band(id, |b| *b = with_kind(b, new_kind), true);
                        }
                    }
                >
                    {options}
                </select>
                <label class="eq-band-on">
                    <input
                        type="checkbox"
                        data-testid="band-on"
                        prop:checked=move || band.get().is_some_and(|b| b.enabled)
                        on:change=move |ev| {
                            let on = event_target_checked(&ev);
                            edit.edit_band(id, |b| b.enabled = on, true);
                        }
                    />
                    " zapnuté"
                </label>
                <button
                    class="eq-band-remove"
                    data-testid="band-remove"
                    on:click=move |_| edit.remove(id)
                >
                    "Odstrániť"
                </button>
            </div>
            {freq}
            {gain}
            {q}
        </div>
    }
}
