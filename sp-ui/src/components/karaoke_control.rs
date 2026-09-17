//! Karaoke live-control panel (#14, bound to the now-playing song in #177).
//!
//! Drives `GET/POST /api/v1/karaoke`. The panel binds to the SELECTED playlist's
//! currently-playing song (`store.selected_playlist` + the `now_playing[]` array
//! the karaoke GET now returns) and shows whether THAT song's karaoke stems are
//! ready: the mode dropdown + vocal-gain slider are DISABLED (with the reason)
//! unless the song's stems are ready, and a "Zaradiť do fronty" button re-enqueues
//! a song whose stems are unavailable/failed. The global done/pending counter
//! moved to the panel-title tooltip. #181 (D2) restyles this later — it MUST keep
//! this state contract.

use leptos::prelude::*;

use crate::api;
use crate::store::DashboardStore;

/// The four karaoke modes as (value, label) pairs. Value matches
/// `sp_core::playback::KaraokeMode::as_str`.
const MODES: &[(&str, &str)] = &[
    ("full_mix", "Plný mix"),
    ("karaoke_low", "Karaoke (stíšené vokály)"),
    ("vocals_only", "Iba vokály"),
    ("instrumental_only", "Iba hudba (karaoke)"),
];

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

#[component]
pub fn KaraokeControl() -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");

    let mode = RwSignal::new("full_mix".to_string());
    let vocal_gain = RwSignal::new(0.3_f32); // 0.0..=1.0
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
    // changes (a new song, or a different selected playlist) so the header + the
    // ready-state stay live as songs advance on the wall. A `Memo` collapses the
    // frequent WebSocket position ticks to only the (playlist, video_id) changes,
    // so this refetches on a song change — not on every progress update.
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

    // Push the current mode + gain to the server.
    let push = move || {
        let m = mode.get_untracked();
        let g = vocal_gain.get_untracked();
        leptos::task::spawn_local(async move {
            match api::post_karaoke(&m, g).await {
                Ok(()) => status.set("Uložené".into()),
                Err(e) => status.set(format!("Chyba: {e}")),
            }
        });
    };

    let on_mode_change = move |ev: leptos::ev::Event| {
        mode.set(event_target_value(&ev));
        push();
    };

    // Live label on drag (`input`); POST on release (`change`).
    let set_gain_from = move |ev: &leptos::ev::Event| {
        if let Ok(pct) = event_target_value(ev).parse::<f32>() {
            vocal_gain.set((pct / 100.0).clamp(0.0, 1.0));
        }
    };
    let on_gain_input = move |ev: leptos::ev::Event| set_gain_from(&ev);
    let on_gain_change = move |ev: leptos::ev::Event| {
        set_gain_from(&ev);
        push();
    };

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

    // Reactive helpers for the disabled/hint state.
    let disabled = move || !is_ready();
    // The header text: which song + its stems state.
    let header = move || match selected_entry() {
        Some(e) => format!("Stemy — {}: {}", e.title, state_label(&e.stems_state, e.queue_position)),
        None => "Stemy — nič nehrá".to_string(),
    };
    // The reason controls are locked (empty when ready).
    let lock_reason = move || match selected_entry() {
        Some(e) if e.stems_state != "ready" => e
            .stems_error
            .clone()
            .unwrap_or_else(|| "táto skladba nemá pripravené stemy".to_string()),
        None => "žiadna skladba nehrá na zvolenom playliste".to_string(),
        _ => String::new(),
    };
    // Show the enqueue button only for unavailable/failed songs.
    let show_enqueue = move || {
        selected_entry()
            .map(|e| e.stems_state == "unavailable" || e.stems_state == "failed")
            .unwrap_or(false)
    };
    let counter_tooltip = move || {
        format!(
            "Stemy v katalógu: {} hotových, {} čaká",
            stems_done.get(),
            stems_pending.get()
        )
    };

    view! {
        <div class="karaoke-control">
            <h3 title=counter_tooltip data-testid="karaoke-title">
                "Karaoke"
            </h3>
            <p class="karaoke-now-playing" data-testid="karaoke-now-playing">
                {header}
            </p>
            <p
                class="karaoke-lock-reason"
                data-testid="karaoke-lock-reason"
                class:hidden=move || !disabled()
            >
                {lock_reason}
            </p>

            <label class:disabled=disabled>
                "Režim"
                <select
                    data-testid="karaoke-mode"
                    prop:disabled=disabled
                    on:change=on_mode_change
                >
                    {MODES
                        .iter()
                        .map(|(val, label)| {
                            let val = val.to_string();
                            view! {
                                <option value=val.clone() selected=move || mode.get() == val>
                                    {*label}
                                </option>
                            }
                        })
                        .collect_view()}
                </select>
            </label>

            // The fader is live in every stem preset when the song is ready;
            // Plný mix labels it a no-op, and a not-ready song disables it (#177).
            <label class:disabled=move || disabled() || mode.get() == "full_mix">
                "Hlasitosť vokálov: "
                {move || format!("{}%", (vocal_gain.get() * 100.0).round() as i32)}
                <input
                    type="range"
                    min="0"
                    max="100"
                    step="1"
                    data-testid="karaoke-vocal-gain"
                    prop:value=move || (vocal_gain.get() * 100.0).round() as i32
                    prop:disabled=move || disabled() || mode.get() == "full_mix"
                    on:input=on_gain_input
                    on:change=on_gain_change
                />
                <span class="karaoke-fader-hint" data-testid="karaoke-fader-hint">
                    {move || {
                        if !is_ready() {
                            ""
                        } else if mode.get() == "full_mix" {
                            " — v Plnom mixe bez efektu"
                        } else {
                            ""
                        }
                    }}
                </span>
            </label>

            <button
                class="karaoke-enqueue"
                data-testid="karaoke-enqueue"
                class:hidden=move || !show_enqueue()
                on:click=on_enqueue
            >
                "Zaradiť do fronty"
            </button>

            <span class="save-status">{move || status.get()}</span>
        </div>
    }
}
