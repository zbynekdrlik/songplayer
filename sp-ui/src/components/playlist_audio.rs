//! #242 / #278: a playlist's own sound — its volume and its parametric EQ,
//! edited live on a real curve (modelled on iemmixer's EQ, design comment
//! 6221171333 on #278).
//!
//! Collapsed under the Player on the playlist card. Opening it reads
//! `GET /api/v1/playlists/{id}/audio` ("Načítavam…" until it lands, the
//! fields only after it, so no edit is overwritten by the load). Then:
//!
//! - the volume, a slider with its value;
//! - the curve (`playlist_audio_curve.rs`): a log grid, the summed curve
//!   with a soft fill, each band's own curve in its type's colour, and one
//!   draggable handle per band (the wheel over it changes Q);
//! - one card per band (`playlist_audio_band.rs`), by frequency: the type,
//!   a log frequency slider, gain, Q, each value shown, on / off, delete;
//!   a double-click on a value resets it to its type's default;
//! - "Pridať pásmo" offers the five types, each born with its own defaults
//!   (`sp_core::audio_fx::default_band`).
//!
//! Every change goes out live as `PUT …/audio` (no "Použiť"): at most one
//! send per 50 ms, the latest settings winning, one request in flight, a
//! gesture's end at once — `sp_core::audio_fx_send::SendGate` decides, this
//! file only carries out its steps. The playing song follows through the
//! server's 50 ms crossfade. All drawing numbers come from
//! `sp_core::audio_fx` / `audio_fx_view`, the server's own formulas.
//!
//! The cards and the handles are keyed by a band id that lives only here
//! (`EqEdit::ids`, parallel to `fx.eq`), so an edit never re-creates the
//! control under the operator and a new order moves the cards.

use std::time::Duration;

use futures::future::{Either, select};
use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;
use serde::Deserialize;
use sp_core::audio_fx::{
    BandKind, EqBand, GAIN_MAX_DB, GAIN_MIN_DB, MAX_BANDS, PlaylistFx, default_band, validate,
};
use sp_core::audio_fx_send::{
    ANSWER_TIMEOUT_MS, LateAnswer, NO_ANSWER_SK, SAVED_SK, SAVING_SK, SendGate, SendStep,
    answer_sk, late_answer,
};
use sp_core::audio_fx_view::{display_order, gain_label, snap_gain};
use wasm_bindgen::JsCast;

use crate::api;
use crate::components::player::now_ms;
use crate::components::playlist_audio_band::band_card;
use crate::components::playlist_audio_curve::eq_curve;

/// `GET …/audio`.
#[derive(Debug, Clone, Deserialize)]
struct AudioView {
    gain_db: f64,
    eq: Vec<EqBand>,
}

/// The panel's edit state, shared by the curve and the band cards. Every
/// field is a reactive handle, so the whole thing is `Copy`.
#[derive(Clone, Copy)]
pub(crate) struct EqEdit {
    playlist_id: i64,
    /// The settings as edited (the server's shape).
    pub(crate) fx: RwSignal<PlaylistFx>,
    /// One id per band of `fx.eq`, in the same order: its card's and its
    /// handle's key.
    pub(crate) ids: RwSignal<Vec<u64>>,
    /// The ids in the cards' order (`display_order`), taken at the load,
    /// an add, a remove and every gesture's end — never during a drag.
    pub(crate) order: RwSignal<Vec<u64>>,
    next_id: StoredValue<u64>,
    gate: StoredValue<SendGate>,
    /// The status line under the panel.
    pub(crate) status: RwSignal<String>,
}

impl EqEdit {
    fn new(playlist_id: i64) -> Self {
        Self {
            playlist_id,
            fx: RwSignal::new(PlaylistFx::default()),
            ids: RwSignal::new(Vec::new()),
            order: RwSignal::new(Vec::new()),
            next_id: StoredValue::new(0),
            gate: StoredValue::new(SendGate::default()),
            status: RwSignal::new(String::new()),
        }
    }

    /// Take the stored settings (the load): fresh ids, the cards' order.
    /// Nothing is sent.
    fn take_stored(self, fx: PlaylistFx) {
        let count = fx.eq.len() as u64;
        self.ids.set((0..count).collect());
        self.next_id.set_value(count);
        self.fx.set(fx);
        self.resort();
    }

    /// The index of band `id` in `fx.eq` (tracked).
    pub(crate) fn index_of(self, id: u64) -> Option<usize> {
        self.ids.with(|ids| ids.iter().position(|i| *i == id))
    }

    /// Band `id` as edited (tracked).
    pub(crate) fn band(self, id: u64) -> Option<EqBand> {
        let index = self.index_of(id)?;
        self.fx.with(|f| f.eq.get(index).copied())
    }

    /// Band `id`'s number on its card and its handle: its place in the
    /// cards' order, from 1 (tracked).
    pub(crate) fn number(self, id: u64) -> Option<usize> {
        self.order
            .with(|order| order.iter().position(|i| *i == id))
            .map(|place| place + 1)
    }

    /// Change band `id`. `finished`: the gesture ended (a release, a
    /// switch, a type, a reset), so the change goes out at once and the
    /// cards take their order; else it is sent throttled.
    pub(crate) fn edit_band(self, id: u64, change: impl FnOnce(&mut EqBand), finished: bool) {
        let index = self
            .ids
            .with_untracked(|ids| ids.iter().position(|i| *i == id));
        let before = index.and_then(|i| self.fx.with_untracked(|f| f.eq.get(i).copied()));
        if let (Some(index), Some(before)) = (index, before) {
            let mut after = before;
            change(&mut after);
            if after != before {
                self.fx.update(|f| {
                    if let Some(band) = f.eq.get_mut(index) {
                        *band = after;
                    }
                });
                self.step(SendGate::changed);
            }
        }
        if finished {
            self.finish();
        }
    }

    /// Change the playlist's volume (`finished` as for a band).
    pub(crate) fn edit_volume(self, db: f64, finished: bool) {
        if self.fx.with_untracked(|f| f.gain_db != db) {
            self.fx.update(|f| f.gain_db = db);
            self.step(SendGate::changed);
        }
        if finished {
            self.finish();
        }
    }

    /// A gesture ended: its last value goes out at once, and the cards
    /// take their order.
    pub(crate) fn finish(self) {
        self.step(SendGate::flush);
        // A card that moves is re-inserted, and the browser takes the focus
        // from a control inside it (a slider or a type chosen with the
        // keyboard): give it back once the cards have moved.
        let focused = document()
            .active_element()
            .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok());
        if self.resort()
            && let Some(el) = focused
        {
            request_animation_frame(move || {
                if el.is_connected() {
                    // Never scroll the page for it.
                    let options = web_sys::FocusOptions::new();
                    options.set_prevent_scroll(true);
                    let _ = el.focus_with_options(&options);
                }
            });
        }
    }

    /// Add a band of `kind` with its type's defaults (none past
    /// [`MAX_BANDS`]).
    fn add(self, kind: BandKind) {
        if self.fx.with_untracked(|f| f.eq.len()) >= MAX_BANDS {
            return;
        }
        let id = self.next_id.get_value();
        self.next_id.set_value(id + 1);
        self.fx.update(|f| f.eq.push(default_band(kind)));
        self.ids.update(|ids| ids.push(id));
        self.step(SendGate::changed);
        self.finish();
    }

    /// Delete band `id`.
    pub(crate) fn remove(self, id: u64) {
        let Some(index) = self
            .ids
            .with_untracked(|ids| ids.iter().position(|i| *i == id))
        else {
            return;
        };
        self.fx.update(|f| {
            if index < f.eq.len() {
                f.eq.remove(index);
            }
        });
        self.ids.update(|ids| {
            ids.remove(index);
        });
        self.step(SendGate::changed);
        self.finish();
    }

    /// The cards' order from the bands as they are now; whether it changed.
    fn resort(self) -> bool {
        let order = self.fx.with_untracked(|f| display_order(&f.eq));
        let ids = self.ids.get_untracked();
        let order: Vec<u64> = order
            .into_iter()
            .filter_map(|i| ids.get(i).copied())
            .collect();
        let changed = self.order.with_untracked(|current| *current != order);
        if changed {
            self.order.set(order);
        }
        changed
    }

    /// Run one decision of the send gate and carry out its step. `try_*`:
    /// a timer or an answer can outlive the panel (navigation).
    fn step(self, decide: impl FnOnce(&mut SendGate, f64) -> SendStep) {
        let Some((step, busy)) = self.gate.try_update_value(|gate| {
            let step = decide(gate, now_ms() as f64);
            (step, gate.busy())
        }) else {
            return;
        };
        if busy {
            let _ = self.status.try_set(SAVING_SK.to_string());
        }
        match step {
            SendStep::Idle => {}
            SendStep::WaitMs(ms) => set_timeout(
                move || self.step(SendGate::tick),
                Duration::from_millis(ms.max(0.0).ceil() as u64),
            ),
            SendStep::Send => self.send(),
        }
    }

    /// Send the settings as they are now (the latest value wins).
    fn send(self) {
        let Some(fx) = self.fx.try_get_untracked() else {
            return;
        };
        if let Err(refusal) = validate(&fx) {
            self.step(SendGate::landed);
            let _ = self.status.try_set(refusal.sk());
            return;
        }
        let path = format!("/api/v1/playlists/{}/audio", self.playlist_id);
        let sent = fx.clone();
        leptos::task::spawn_local(async move {
            // An answer that never comes must not hold the gate in flight:
            // it is given up after `ANSWER_TIMEOUT_MS`, and the request is
            // still watched (it may land later, over a newer save).
            let put = Box::pin(async move { api::put_json_empty(&path, &fx).await });
            let give_up = Box::pin(TimeoutFuture::new(ANSWER_TIMEOUT_MS));
            let text = match select(put, give_up).await {
                Either::Left((answer, _)) => {
                    self.step(SendGate::landed);
                    let busy = self.gate.try_with_value(SendGate::busy).unwrap_or(false);
                    answer_sk(&answer, busy)
                }
                Either::Right((_, put)) => {
                    self.step(SendGate::landed);
                    leptos::task::spawn_local(self.watch_late_answer(put, sent));
                    NO_ANSWER_SK.to_string()
                }
            };
            let _ = self.status.try_set(text);
        });
    }

    /// A send given up that still lands: `sp_core`'s `late_answer` decides
    /// (newer settings it may have landed over go out once more; unchanged
    /// ones it saved after all).
    async fn watch_late_answer(
        self,
        put: impl std::future::Future<Output = Result<(), String>>,
        sent: PlaylistFx,
    ) {
        let landed_ok = put.await.is_ok();
        let Some(now) = self.fx.try_get_untracked() else {
            return;
        };
        let Some(busy) = self.gate.try_with_value(SendGate::busy) else {
            return;
        };
        match late_answer(landed_ok, now != sent, busy) {
            LateAnswer::Resend => self.step(SendGate::changed),
            LateAnswer::Saved => {
                let _ = self.status.try_set(SAVED_SK.to_string());
            }
            LateAnswer::Nothing => {}
        }
    }
}

/// The volume row: a slider over the whole allowed range, its value, a
/// double-click back to 0 dB.
fn volume_row(edit: EqEdit) -> impl IntoView {
    let gain = Memo::new(move |_| edit.fx.with(|f| f.gain_db));
    let slid = move |ev: leptos::ev::Event| {
        if let Ok(db) = event_target_value(&ev).parse::<f64>() {
            edit.edit_volume(snap_gain(db), false);
        }
    };
    view! {
        <div
            class="eq-param eq-volume"
            title="Dvojklik vráti hlasitosť na 0 dB"
            on:dblclick=move |_| edit.edit_volume(0.0, true)
        >
            <span class="eq-param-label">"Hlasitosť"</span>
            <input
                type="range"
                data-testid="playlist-audio-gain"
                min=GAIN_MIN_DB
                max=GAIN_MAX_DB
                step="0.1"
                prop:value=move || format!("{:.1}", gain.get())
                on:input=slid
                on:change=move |_| edit.finish()
                on:pointerup=move |_| edit.finish()
            />
            <span class="eq-param-value" data-testid="playlist-audio-gain-value">
                {move || gain_label(gain.get())}
            </span>
        </div>
    }
}

/// "Pridať pásmo": one button per type, low to high.
fn add_row(edit: EqEdit) -> impl IntoView {
    let full = Memo::new(move |_| edit.fx.with(|f| f.eq.len()) >= MAX_BANDS);
    let buttons = BandKind::ALL
        .into_iter()
        .map(|kind| {
            view! {
                <button
                    class="eq-add"
                    data-testid=format!("playlist-audio-add-{}", kind.name())
                    prop:disabled=move || full.get()
                    on:click=move |_| edit.add(kind)
                >
                    {format!("+ {}", kind.label_sk())}
                </button>
            }
        })
        .collect_view();
    view! {
        <div class="playlist-audio-actions">
            <span class="eq-add-label">"Pridať pásmo:"</span>
            {buttons}
        </div>
    }
}

#[component]
pub fn PlaylistAudio(playlist_id: i64) -> impl IntoView {
    let open = RwSignal::new(false);
    let loaded = RwSignal::new(false);
    let edit = EqEdit::new(playlist_id);

    let load = move || {
        leptos::task::spawn_local(async move {
            let path = format!("/api/v1/playlists/{playlist_id}/audio");
            match api::get::<AudioView>(&path).await {
                Ok(view) => {
                    // Gone (navigation), or a load started earlier (the
                    // panel closed and opened again before it landed)
                    // already landed: never overwrite what was edited since.
                    if loaded.try_get_untracked() != Some(false) {
                        return;
                    }
                    edit.take_stored(PlaylistFx {
                        gain_db: view.gain_db,
                        eq: view.eq,
                    });
                    let _ = loaded.try_set(true);
                    let _ = edit.status.try_set(String::new());
                }
                Err(e) => {
                    let _ = edit.status.try_set(format!("Načítanie zvuku zlyhalo: {e}"));
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

    view! {
        <div class="playlist-audio">
            <button
                class="playlist-audio-toggle"
                data-testid="playlist-audio-toggle"
                on:click=toggle
            >
                {move || if open.get() { "▼ Zvuk playlistu" } else { "▶ Zvuk playlistu" }}
            </button>
            // The fields only once the stored sound is here: an edit made
            // before would be overwritten by the load (CI run 37975959175).
            {move || {
                (open.get() && !loaded.get())
                    .then(|| {
                        view! {
                            <span class="playlist-audio-status" data-testid="playlist-audio-loading">
                                {move || {
                                    let text = edit.status.get();
                                    if text.is_empty() { "Načítavam…".to_string() } else { text }
                                }}
                            </span>
                        }
                    })
            }}
            {move || {
                (open.get() && loaded.get())
                    .then(|| {
                        view! {
                            <div class="playlist-audio-body">
                                {volume_row(edit)}
                                {eq_curve(edit)}
                                <div class="curve-legend">
                                    "Bod pásma ťahaj myšou: doľava a doprava frekvencia, hore a dole zisk; koliesko nad bodom mení Q. Dvojklik na hodnotu ju vráti na predvolenú. Každá zmena sa okamžite uloží a hneď ju počuť."
                                </div>
                                <div class="playlist-audio-bands">
                                    <For
                                        each=move || edit.order.get()
                                        key=|id: &u64| *id
                                        children=move |id: u64| band_card(edit, id)
                                    />
                                </div>
                                {add_row(edit)}
                                <span
                                    class="playlist-audio-status"
                                    data-testid="playlist-audio-status"
                                >
                                    {move || edit.status.get()}
                                </span>
                            </div>
                        }
                    })
            }}
        </div>
    }
}
