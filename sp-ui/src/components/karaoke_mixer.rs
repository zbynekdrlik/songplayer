//! Song (karaoke) adapter for the modern mixer (#181 D2). Replaces
//! `karaoke_control.rs`: same live control over `GET/POST /api/v1/karaoke` and
//! the same per-song stems state contract (#177), rendered through the shared
//! `Mixer`. Two faders — the live `vokál` (= `vocal_gain`) and a read-only
//! `inštrumentál` (fixed 100 %, "pevné", until the API exposes a second live
//! gain) — plus the four karaoke-mode presets. The mode `<select>` became the
//! preset row; the `data-testid`s `karaoke-mode` (presets group) and
//! `karaoke-vocal-gain` (vocal fader) and the `karaoke-now-playing` state line
//! are preserved so the post-deploy + mock E2E keep working.

use leptos::prelude::*;
use sp_core::mixer_model::{MixerKind, channel_labels, presets};

use crate::api;
use crate::components::mixer::{Mixer, PresetSpec};
use crate::components::mixer_channel::ChannelSpec;
use crate::store::DashboardStore;

/// Per-playing-song stems state parsed from `GET /api/v1/karaoke` → `now_playing[]`.
#[derive(Clone, Debug, Default, PartialEq)]
struct NowPlayingStems {
    playlist_id: i64,
    title: String,
    stems_state: String,
    stems_error: Option<String>,
    queue_position: Option<i64>,
    video_id: i64,
}

/// Header glyph + label for a stems state (#177).
fn state_label(state: &str, queue_position: Option<i64>) -> String {
    match state {
        "ready" => "● pripravené".to_string(),
        "processing" => "⚙ spracúvam".to_string(),
        "queued" => match queue_position {
            Some(n) => format!("⏳ vo fronte ({n}.)"),
            None => "⏳ vo fronte".to_string(),
        },
        "unavailable" => "— nedostupné".to_string(),
        "failed" => "✖ chyba".to_string(),
        _ => "—".to_string(),
    }
}

/// POST the current mode + vocal gain to the server (live — no pipeline reopen,
/// #186). Signals are `Copy`, so each callback captures them independently.
fn post_state(mode: RwSignal<String>, vocal_gain: RwSignal<f32>, status: RwSignal<String>) {
    let m = mode.get_untracked();
    let g = vocal_gain.get_untracked();
    leptos::task::spawn_local(async move {
        match api::post_karaoke(&m, g).await {
            Ok(()) => status.set("Uložené".into()),
            Err(e) => status.set(format!("Chyba: {e}")),
        }
    });
}

#[component]
pub fn KaraokeMixer() -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");

    let mode = RwSignal::new("full_mix".to_string());
    let vocal_gain = RwSignal::new(0.3_f32); // 0.0..=1.0
    let instrumental = RwSignal::new(1.0_f32); // read-only display: fixed 100 %
    let stems_done = RwSignal::new(0_i64);
    let stems_pending = RwSignal::new(0_i64);
    let now_playing = RwSignal::new(Vec::<NowPlayingStems>::new());
    let status = RwSignal::new(String::new());

    // Load the current live state + per-song stems from the karaoke GET.
    let load = move || {
        leptos::task::spawn_local(async move {
            if let Ok(v) = api::get_karaoke().await {
                if let Some(m) = v.get("mode").and_then(|x| x.as_str()) {
                    mode.set(m.to_string());
                }
                if let Some(g) = v.get("vocal_gain").and_then(|x| x.as_f64()) {
                    vocal_gain.set(g as f32);
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
                                playlist_id: e.get("playlist_id").and_then(|x| x.as_i64()).unwrap_or(0),
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
    // Reload on mount AND whenever the selected playlist's now-playing SONG
    // changes (a `Memo` collapses the frequent position ticks to song changes).
    let selected_song = Memo::new(move |_| {
        let sel = store.selected_playlist.get()?;
        store.now_playing.get().get(&sel).map(|n| n.video_id)
    });
    Effect::new(move |_| {
        let _ = selected_song.get();
        load();
    });

    // The now-playing stems entry for the SELECTED playlist (if any).
    let selected_entry = move || {
        let sel = store.selected_playlist.get()?;
        now_playing.get().into_iter().find(|e| e.playlist_id == sel)
    };
    let is_ready = move || selected_entry().map(|e| e.stems_state == "ready").unwrap_or(false);

    // "Zaradiť do fronty": re-enqueue the selected song's stems.
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

    // Reactive derivations for the shared Mixer.
    let title = Signal::derive(move || {
        format!(
            "Mixér skladby · {} stemov hotových, {} čaká",
            stems_done.get(),
            stems_pending.get()
        )
    });
    let state_line = Signal::derive(move || match selected_entry() {
        Some(e) => format!("Stemy — {}: {}", e.title, state_label(&e.stems_state, e.queue_position)),
        None => "Stemy — nič nehrá".to_string(),
    });
    let disabled_reason = Signal::derive(move || {
        if is_ready() {
            return None;
        }
        Some(match selected_entry() {
            Some(e) => e
                .stems_error
                .clone()
                .unwrap_or_else(|| "táto skladba nemá pripravené stemy".to_string()),
            None => "žiadna skladba nehrá na zvolenom playliste".to_string(),
        })
    });
    let active_preset = Signal::derive(move || Some(mode.get()));
    let show_enqueue = move || {
        selected_entry()
            .map(|e| e.stems_state == "unavailable" || e.stems_state == "failed")
            .unwrap_or(false)
    };

    // Channels: the live vocal fader (disabled in Plný mix, #186) + the read-only
    // instrumental (fixed 100 %, honest "pevné" until the API exposes it).
    let labels = channel_labels(MixerKind::Song);
    let vocal_enabled = Signal::derive(move || is_ready() && mode.get() != "full_mix");
    let channels = vec![
        ChannelSpec {
            label: labels[0].to_string(),
            gain: vocal_gain,
            enabled: vocal_enabled,
            fixed_note: None,
            on_change: Callback::new(move |_v: f32| post_state(mode, vocal_gain, status)),
            testid: Some("karaoke-vocal-gain".to_string()),
        },
        ChannelSpec {
            label: labels[1].to_string(),
            gain: instrumental,
            enabled: Signal::derive(|| false),
            fixed_note: Some("pevné".to_string()),
            on_change: Callback::new(|_v: f32| {}),
            testid: None,
        },
    ];

    // Presets: the four karaoke modes; picking one sets the mode + POSTs it.
    let preset_specs: Vec<PresetSpec> = presets(MixerKind::Song)
        .iter()
        .map(|p| {
            let id = p.id.to_string();
            let id_for_click = id.clone();
            PresetSpec {
                id,
                label: p.label.to_string(),
                on_select: Callback::new(move |_| {
                    mode.set(id_for_click.clone());
                    post_state(mode, vocal_gain, status);
                }),
            }
        })
        .collect();

    view! {
        <Mixer
            title=title
            state_line=state_line
            state_testid="karaoke-now-playing"
            disabled_reason=disabled_reason
            channels=channels
            presets=preset_specs
            active_preset=active_preset
            presets_testid="karaoke-mode"
            extra_class="mixer-karaoke"
        >
            <button
                class="mixer-enqueue"
                data-testid="karaoke-enqueue"
                class:hidden=move || !show_enqueue()
                on:click=on_enqueue
            >
                "Zaradiť do fronty"
            </button>
            <span class="mixer-status">{move || status.get()}</span>
        </Mixer>
    }
}
