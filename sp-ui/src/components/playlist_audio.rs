//! #242: a playlist's own sound — its volume and its EQ, with the curve.
//!
//! Collapsed under the Player on the playlist card. Opening it reads
//! `GET /api/v1/playlists/{id}/audio`; "Použiť" checks the limits with
//! `sp_core::audio_fx::validate` (the Slovak reason, `FxError::sk`) and
//! sends `PUT …/audio`, which the playing song follows at its next audio
//! chunk. The curve is `sp_core::audio_fx::curve_path`, the server's maths.
//!
//! The band rows are rebuilt only when the NUMBER of bands changes; each
//! field reads its band by index, so an edit never re-creates the input
//! under the operator (focus stays where it is).

use leptos::prelude::*;
use serde::Deserialize;
use sp_core::audio_fx::{
    BandKind, CURVE_DB_MAX, CURVE_DB_MIN, DEFAULT_Q, EqBand, MAX_BANDS, PlaylistFx, curve_path,
    db_y, freq_x, validate,
};

use crate::api;

/// The curve box, px.
const CURVE_W: f64 = 420.0;
const CURVE_H: f64 = 150.0;

/// `GET …/audio`.
#[derive(Debug, Clone, Deserialize)]
struct AudioView {
    gain_db: f64,
    eq: Vec<EqBand>,
}

/// Each band type: its JSON name and its Slovak label.
const KINDS: [(BandKind, &str, &str); 5] = [
    (
        BandKind::HighPass,
        "high_pass",
        "Orezanie basov (horná priepusť)",
    ),
    (BandKind::LowShelf, "low_shelf", "Basy (šelf)"),
    (BandKind::Peak, "peak", "Pásmo (zvon)"),
    (BandKind::HighShelf, "high_shelf", "Výšky (šelf)"),
    (
        BandKind::LowPass,
        "low_pass",
        "Orezanie výšok (dolná priepusť)",
    ),
];

fn kind_name(kind: BandKind) -> &'static str {
    KINDS
        .iter()
        .find(|(k, _, _)| *k == kind)
        .map(|(_, name, _)| *name)
        .unwrap_or("peak")
}

fn kind_of(name: &str) -> BandKind {
    KINDS
        .iter()
        .find(|(_, n, _)| *n == name)
        .map(|(k, _, _)| *k)
        .unwrap_or(BandKind::Peak)
}

/// A number from an input, or `None` (the field keeps its last value).
fn number(value: &str) -> Option<f64> {
    value.trim().replace(',', ".").parse::<f64>().ok()
}

/// A number as an input shows it.
fn shown(value: f64) -> String {
    let rounded = (value * 100.0).round() / 100.0;
    rounded.to_string()
}

/// One band row: every field reads band `i` of `fx` and writes it back.
fn band_row(fx: RwSignal<PlaylistFx>, i: usize) -> impl IntoView {
    let read = move |get: fn(&EqBand) -> f64| {
        fx.with(|f| f.eq.get(i).map(get).map(shown).unwrap_or_default())
    };
    let kind = move || fx.with(|f| f.eq.get(i).map(|b| b.kind));
    let write = move |change: fn(&mut EqBand, f64), ev: leptos::ev::Event| {
        if let Some(n) = number(&event_target_value(&ev)) {
            fx.update(|f| {
                if let Some(b) = f.eq.get_mut(i) {
                    change(b, n);
                }
            });
        }
    };
    view! {
        <div class="playlist-audio-band" data-testid="playlist-audio-band">
            <select
                data-testid="band-kind"
                prop:value=move || kind().map(kind_name).unwrap_or("peak")
                on:change=move |ev| {
                    let k = kind_of(&event_target_value(&ev));
                    fx.update(|f| {
                        if let Some(b) = f.eq.get_mut(i) {
                            b.kind = k;
                        }
                    });
                }
            >
                {KINDS
                    .iter()
                    .map(|(k, name, label)| {
                        let k = *k;
                        view! {
                            <option value={*name} selected=move || kind() == Some(k)>
                                {*label}
                            </option>
                        }
                    })
                    .collect_view()}
            </select>
            <label>
                "Frekvencia (Hz) "
                <input
                    type="number"
                    data-testid="band-freq"
                    min="20"
                    max="20000"
                    step="1"
                    prop:value=move || read(|b| b.freq_hz)
                    on:change=move |ev| write(|b, n| b.freq_hz = n, ev)
                />
            </label>
            <label>
                "Zisk (dB) "
                <input
                    type="number"
                    data-testid="band-gain"
                    min="-30"
                    max="12"
                    step="0.5"
                    prop:disabled=move || !kind().is_some_and(|k| k.uses_gain())
                    prop:value=move || read(|b| b.gain_db)
                    on:change=move |ev| write(|b, n| b.gain_db = n, ev)
                />
            </label>
            <label>
                "Q "
                <input
                    type="number"
                    data-testid="band-q"
                    min="0.1"
                    max="10"
                    step="0.1"
                    prop:value=move || read(|b| b.q)
                    on:change=move |ev| write(|b, n| b.q = n, ev)
                />
            </label>
            <label>
                <input
                    type="checkbox"
                    data-testid="band-on"
                    prop:checked=move || fx.with(|f| f.eq.get(i).is_some_and(|b| b.enabled))
                    on:change=move |ev| {
                        let on = event_target_checked(&ev);
                        fx.update(|f| {
                            if let Some(b) = f.eq.get_mut(i) {
                                b.enabled = on;
                            }
                        });
                    }
                />
                " zapnuté"
            </label>
            <button
                data-testid="band-remove"
                on:click=move |_| {
                    fx.update(|f| {
                        if i < f.eq.len() {
                            f.eq.remove(i);
                        }
                    })
                }
            >
                "Odstrániť"
            </button>
        </div>
    }
}

/// The curve's grid: 100 Hz, 1 kHz, 10 kHz and the 0 dB line.
fn curve_grid() -> impl IntoView {
    let zero = db_y(0.0, CURVE_H);
    let verticals = [100.0, 1000.0, 10_000.0]
        .iter()
        .map(|f| {
            let x = freq_x(*f, CURVE_W);
            view! { <line class="curve-grid" x1=x y1=0.0 x2=x y2=CURVE_H /> }
        })
        .collect_view();
    view! {
        {verticals}
        <line class="curve-zero" x1=0.0 y1=zero x2=CURVE_W y2=zero />
    }
}

#[component]
pub fn PlaylistAudio(playlist_id: i64) -> impl IntoView {
    let open = RwSignal::new(false);
    let loaded = RwSignal::new(false);
    let fx = RwSignal::new(PlaylistFx::default());
    let status = RwSignal::new(String::new());
    let bands = Memo::new(move |_| fx.with(|f| f.eq.len()));

    let load = move || {
        leptos::task::spawn_local(async move {
            let path = format!("/api/v1/playlists/{playlist_id}/audio");
            match api::get::<AudioView>(&path).await {
                Ok(view) => {
                    let _ = fx.try_set(PlaylistFx {
                        gain_db: view.gain_db,
                        eq: view.eq,
                    });
                    let _ = loaded.try_set(true);
                    let _ = status.try_set(String::new());
                }
                Err(e) => {
                    let _ = status.try_set(format!("Načítanie zvuku zlyhalo: {e}"));
                }
            }
        });
    };
    let toggle = move |_| {
        let now = !open.get_untracked();
        open.set(now);
        if now && !loaded.get_untracked() {
            load();
        }
    };
    let apply = move |_| {
        if !loaded.get_untracked() {
            status.set("Zvuk sa ešte nenačítal — nič neukladám".to_string());
            return;
        }
        let current = fx.get_untracked();
        if let Err(e) = validate(&current) {
            status.set(e.sk());
            return;
        }
        status.set("Ukladám…".to_string());
        leptos::task::spawn_local(async move {
            let path = format!("/api/v1/playlists/{playlist_id}/audio");
            let text = match api::put_json_empty(&path, &current).await {
                Ok(()) => "Použité".to_string(),
                Err(e) => format!("Uloženie zlyhalo: {e}"),
            };
            let _ = status.try_set(text);
        });
    };
    let add_band = move |_| {
        fx.update(|f| {
            if f.eq.len() < MAX_BANDS {
                f.eq.push(EqBand {
                    kind: BandKind::Peak,
                    freq_hz: 1000.0,
                    gain_db: 0.0,
                    q: DEFAULT_Q,
                    enabled: true,
                });
            }
        });
    };
    let set_gain = move |ev: leptos::ev::Event| {
        if let Some(v) = number(&event_target_value(&ev)) {
            fx.update(|f| f.gain_db = v);
        }
    };
    let legend =
        format!("20 Hz – 20 kHz · {CURVE_DB_MIN} až {CURVE_DB_MAX:+} dB · vodorovná čiara = 0 dB");

    view! {
        <div class="playlist-audio">
            <button
                class="playlist-audio-toggle"
                data-testid="playlist-audio-toggle"
                on:click=toggle
            >
                {move || if open.get() { "▼ Zvuk playlistu" } else { "▶ Zvuk playlistu" }}
            </button>
            {move || {
                open.get()
                    .then(|| {
                        view! {
                            <div class="playlist-audio-body">
                                <label class="playlist-audio-gain">
                                    "Hlasitosť (dB) "
                                    <input
                                        type="number"
                                        data-testid="playlist-audio-gain"
                                        min="-30"
                                        max="12"
                                        step="0.5"
                                        prop:value=move || shown(fx.with(|f| f.gain_db))
                                        on:change=set_gain
                                    />
                                </label>
                                <svg
                                    class="playlist-audio-curve"
                                    data-testid="playlist-audio-curve"
                                    width=CURVE_W
                                    height=CURVE_H
                                    viewBox=format!("0 0 {CURVE_W} {CURVE_H}")
                                >
                                    {curve_grid()}
                                    <path
                                        class="curve-line"
                                        data-testid="playlist-audio-path"
                                        d=move || fx.with(|f| curve_path(f, CURVE_W, CURVE_H, 160))
                                    />
                                </svg>
                                <div class="curve-legend">{legend.clone()}</div>
                                <div class="playlist-audio-bands">
                                    {move || {
                                        (0..bands.get()).map(|i| band_row(fx, i)).collect_view()
                                    }}
                                </div>
                                <div class="playlist-audio-actions">
                                    <button
                                        data-testid="playlist-audio-add"
                                        prop:disabled=move || { bands.get() >= MAX_BANDS }
                                        on:click=add_band
                                    >
                                        "+ Pridať pásmo"
                                    </button>
                                    <button data-testid="playlist-audio-save" on:click=apply>
                                        "Použiť"
                                    </button>
                                    <span
                                        class="playlist-audio-status"
                                        data-testid="playlist-audio-status"
                                    >
                                        {move || status.get()}
                                    </span>
                                </div>
                            </div>
                        }
                    })
            }}
        </div>
    }
}
