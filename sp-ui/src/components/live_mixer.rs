//! The ONE live mixer adapter (#184 round G). Replaces `karaoke_mixer.rs` +
//! `dub_mixer.rs`: ONE strip of three independent faders `[vokály, podklad,
//! dabing]` over the presentational `Mixer`, driven by `GET/PATCH /api/v1/mix`.
//!
//! The three faders ARE the state (`sp_core::mixer_model`): a fader move PATCHes
//! its own field; the server derives the per-stream gains and every playing mixer
//! ramps with no reopen. Which faders are LIVE follows the PLAYING item — `vokály`
//! is live with stems OR a not-yet-separated dub; `podklad` only with stems (else
//! locked "po separácii"); `dabing` is shown + live only for a READY dub. Presets
//! are fader snapshots (the four song presets always, the three dub presets only
//! with a ready dub). Rendered identically on Dashboard, Live and Dabing (#194).

use leptos::prelude::*;
use sp_core::mixer_model::{
    MixFaders, apply_preset, fader_availability, preset_for_faders, presets,
};

use crate::api;
use crate::components::mixer::{Mixer, PresetSpec};
use crate::components::mixer_channel::ChannelSpec;
use crate::store::DashboardStore;

/// Per-playing-song stems state parsed from `GET /api/v1/mix` → `now_playing[]`.
#[derive(Clone, Debug, Default, PartialEq)]
struct NowPlayingStems {
    playlist_id: i64,
    title: String,
    stems_state: String,
    stems_error: Option<String>,
    queue_position: Option<i64>,
    video_id: i64,
}

/// Header label for a stems state (#177) — reuses the ONE shared status vocabulary.
fn state_label(state: &str, queue_position: Option<i64>) -> String {
    let pos = queue_position.map(|n| n.max(0) as u32);
    sp_core::status_chip::stems_chip(Some(state), pos).label_sk
}

/// PATCH a partial mix body live; raise `in_flight` so the reload Effect can't
/// snap a fader back while the change is landing (#184 round B).
fn send_patch(body: serde_json::Value, in_flight: RwSignal<bool>, status: RwSignal<String>) {
    in_flight.set(true);
    leptos::task::spawn_local(async move {
        match api::patch_mix(body).await {
            Ok(()) => status.set("Uložené".into()),
            Err(e) => status.set(format!("Chyba: {e}")),
        }
        in_flight.set(false);
    });
}

#[component]
pub fn LiveMixer(playlist_id: i64) -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");

    // Fader signals — seeded from GET /api/v1/mix (the global console).
    let vokaly = RwSignal::new(1.0_f32);
    let podklad = RwSignal::new(1.0_f32);
    let dabing = RwSignal::new(1.0_f32);
    let stems_done = RwSignal::new(0_i64);
    let stems_pending = RwSignal::new(0_i64);
    let now_playing = RwSignal::new(Vec::<NowPlayingStems>::new());
    let status = RwSignal::new(String::new());
    // #184 round B: true while a PATCH is in flight — the reload must not overwrite.
    let in_flight = RwSignal::new(false);

    let load = move || {
        leptos::task::spawn_local(async move {
            if let Ok(v) = api::get_mix().await {
                // Do not overwrite the faders mid-PATCH (avoids a snap-back).
                if !in_flight.get_untracked() {
                    if let Some(x) = v.get("vokaly").and_then(|x| x.as_f64()) {
                        vokaly.set(x as f32);
                    }
                    if let Some(x) = v.get("podklad").and_then(|x| x.as_f64()) {
                        podklad.set(x as f32);
                    }
                    if let Some(x) = v.get("dabing").and_then(|x| x.as_f64()) {
                        dabing.set(x as f32);
                    }
                }
                if let Some(d) = v.get("stems_done").and_then(|x| x.as_i64()) {
                    stems_done.set(d);
                }
                if let Some(p) = v.get("stems_pending").and_then(|x| x.as_i64()) {
                    stems_pending.set(p);
                }
                let list = v
                    .get("now_playing")
                    .and_then(|x| x.as_array())
                    .map(|arr| {
                        arr.iter()
                            .map(|e| NowPlayingStems {
                                playlist_id: e
                                    .get("playlist_id")
                                    .and_then(|x| x.as_i64())
                                    .unwrap_or(0),
                                video_id: e.get("video_id").and_then(|x| x.as_i64()).unwrap_or(0),
                                title: e
                                    .get("title")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or_default()
                                    .to_string(),
                                stems_state: e
                                    .get("stems_state")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                stems_error: e
                                    .get("stems_error")
                                    .and_then(|x| x.as_str())
                                    .map(str::to_string),
                                queue_position: e.get("queue_position").and_then(|x| x.as_i64()),
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                now_playing.set(list);
            }
        });
    };

    // Reload on mount AND whenever THIS playlist's playing SONG changes (a Memo
    // collapses the frequent position ticks to song changes).
    let selected_song = Memo::new(move |_| {
        store
            .now_playing
            .get()
            .get(&playlist_id)
            .map(|n| n.video_id)
    });

    // The now-playing stems entry for THIS playlist (if any).
    let selected_entry = move || {
        now_playing
            .get()
            .into_iter()
            .find(|e| e.playlist_id == playlist_id)
    };

    // The playing DubRow (from the app-wide store.dabing, matched to the playing
    // video). A dub row present means the item is a dub video.
    let dub_row = Memo::new(move |_| {
        let vid = store
            .now_playing
            .get()
            .get(&playlist_id)
            .map(|i| i.video_id);
        vid.and_then(|v| store.dabing.get().into_iter().find(|r| r.video_id == v))
    });

    // Readiness: a dub video reads its DubRow; a plain song reads the mix
    // now-playing stems state (the #177 contract).
    let stems_ready = move || match dub_row.get() {
        Some(r) => r.stem_status.as_deref() == Some("done"),
        None => selected_entry()
            .map(|e| e.stems_state == "ready")
            .unwrap_or(false),
    };
    let dub_ready = move || {
        dub_row
            .get()
            .map(|r| r.dub_status == "ready")
            .unwrap_or(false)
    };
    let has_dub_row = move || dub_row.get().is_some();

    // #184 round G1: the playing item's KIND (a READY dub → the dub console, else
    // the song console). The server switches the active memory at the item's open,
    // so the strip must RE-READ GET /mix when the kind flips — a Memo<bool> so the
    // reload fires only on a real kind change, never on a position tick.
    let is_dub = Memo::new(move |_| {
        dub_row
            .get()
            .map(|r| r.dub_status == "ready")
            .unwrap_or(false)
    });
    // Reload GET /mix on mount, whenever THIS playlist's playing SONG changes, AND
    // whenever the item's KIND flips — so the faders snap to the active (song/dub)
    // memory. Both deps are Memos, so a position tick never triggers a reload.
    Effect::new(move |_| {
        let _ = selected_song.get();
        let _ = is_dub.get();
        load();
    });

    // Reactive text/lock signals passed to the shared Mixer (update without a
    // channel rebuild).
    let title = Signal::derive(move || {
        format!(
            "Mixér · {} stemov hotových, {} čaká",
            stems_done.get(),
            stems_pending.get()
        )
    });
    let state_line = Signal::derive(move || {
        if dub_ready() {
            "Dabing pripravený — namixuj vokály, podklad a dabing".to_string()
        } else if has_dub_row() {
            "Dabing sa pripravuje…".to_string()
        } else {
            match selected_entry() {
                Some(e) => format!(
                    "Stemy — {}: {}",
                    e.title,
                    state_label(&e.stems_state, e.queue_position)
                ),
                None => "Stemy — nič nehrá".to_string(),
            }
        }
    });
    // The whole strip is LOCKED only when NOTHING is adjustable (no stems, no ready
    // dub). A no-stems dub is NOT locked — its `podklad` fader is individually
    // disabled with a "po separácii" note while vokály + dabing stay live.
    let disabled_reason = Signal::derive(move || {
        if stems_ready() || dub_ready() {
            return None;
        }
        if has_dub_row() {
            Some("dabing ešte nie je vygenerovaný".to_string())
        } else {
            Some(match selected_entry() {
                Some(e) => e
                    .stems_error
                    .clone()
                    .unwrap_or_else(|| "táto skladba nemá pripravené stemy".to_string()),
                None => "žiadna skladba nehrá na zvolenom playliste".to_string(),
            })
        }
    });

    let show_enqueue = move || {
        dub_row.get().is_none()
            && selected_entry()
                .map(|e| e.stems_state == "unavailable" || e.stems_state == "failed")
                .unwrap_or(false)
    };
    let on_enqueue = move |_| {
        let Some(entry) = selected_entry() else {
            return;
        };
        let vid = entry.video_id;
        leptos::task::spawn_local(async move {
            match api::post_enqueue_stems(vid).await {
                Ok(()) => {
                    status.set("Zaradené do fronty".into());
                    load();
                }
                Err(e) => status.set(format!("Chyba: {e}")),
            }
        });
    };

    // The fader SHAPE (which faders are live) — a Memo so a position tick never
    // rebuilds the channel strip (only a real readiness change does, #194).
    let shape = Memo::new(move |_| (stems_ready(), dub_ready()));

    view! {
        <div class="live-mixer">
            {move || {
                let (sr, dr) = shape.get();
                let avail = fader_availability(sr, dr);

                let vokaly_ch = ChannelSpec {
                    label: "vokály".to_string(),
                    gain: vokaly,
                    enabled: Signal::derive(move || avail.vokaly),
                    fixed_note: None,
                    on_change: Callback::new(move |v: f32| {
                        send_patch(serde_json::json!({ "vokaly": v }), in_flight, status)
                    }),
                    testid: Some("mix-vokaly".to_string()),
                };
                let podklad_ch = ChannelSpec {
                    label: "podklad".to_string(),
                    gain: podklad,
                    enabled: Signal::derive(move || avail.podklad),
                    fixed_note: if avail.podklad {
                        None
                    } else {
                        Some("po separácii".to_string())
                    },
                    on_change: Callback::new(move |v: f32| {
                        send_patch(serde_json::json!({ "podklad": v }), in_flight, status)
                    }),
                    testid: Some("mix-podklad".to_string()),
                };
                let mut channels = vec![vokaly_ch, podklad_ch];
                if dr {
                    channels.push(ChannelSpec {
                        label: "dabing".to_string(),
                        gain: dabing,
                        enabled: Signal::derive(move || avail.dabing),
                        fixed_note: None,
                        on_change: Callback::new(move |v: f32| {
                            send_patch(serde_json::json!({ "dabing": v }), in_flight, status)
                        }),
                        testid: Some("mix-dabing".to_string()),
                    });
                }

                let preset_specs: Vec<PresetSpec> = presets(dr)
                    .into_iter()
                    .map(|p| PresetSpec {
                        id: p.id.to_string(),
                        label: p.label.to_string(),
                        on_select: Callback::new(move |_| {
                            let cur = MixFaders::new(
                                vokaly.get_untracked(),
                                podklad.get_untracked(),
                                dabing.get_untracked(),
                            );
                            let nf = apply_preset(&p, cur);
                            vokaly.set(nf.vokaly);
                            podklad.set(nf.podklad);
                            let body = if p.dabing.is_some() {
                                dabing.set(nf.dabing);
                                serde_json::json!({
                                    "vokaly": nf.vokaly,
                                    "podklad": nf.podklad,
                                    "dabing": nf.dabing,
                                })
                            } else {
                                serde_json::json!({ "vokaly": nf.vokaly, "podklad": nf.podklad })
                            };
                            send_patch(body, in_flight, status);
                        }),
                    })
                    .collect();

                let active_preset = Signal::derive(move || {
                    preset_for_faders(
                        MixFaders::new(vokaly.get(), podklad.get(), dabing.get()),
                        dr,
                    )
                    .map(str::to_string)
                });

                view! {
                    <Mixer
                        title=title
                        state_line=state_line
                        state_testid="karaoke-now-playing"
                        disabled_reason=disabled_reason
                        channels=channels
                        presets=preset_specs
                        active_preset=active_preset
                        presets_testid="mix-presets"
                        extra_class="mixer-live"
                    >
                        <button
                            class="mixer-enqueue"
                            data-testid="mix-enqueue"
                            class:hidden=move || !show_enqueue()
                            on:click=on_enqueue
                        >
                            "Zaradiť do fronty"
                        </button>
                        <span class="mixer-status">{move || status.get()}</span>
                    </Mixer>
                }
            }}
        </div>
    }
}
