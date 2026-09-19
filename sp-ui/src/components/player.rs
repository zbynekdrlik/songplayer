//! #194: the ONE playback surface of the app.
//!
//! `Player(playlist_id)` is rendered identically wherever something can play —
//! every Dashboard card, the Live page, and the Dabing page. It composes the
//! existing pieces (now-playing + on/off-program badge, a real seek bar with
//! ±10 s, transport, playback-mode select, the click-to-start live A/V preview,
//! and the shared `Mixer` through the karaoke or dub adapter chosen from the
//! PLAYING item) so the three pages can never drift apart again.
//!
//! Every test id is set INSIDE this component (`player-*`), never injected by a
//! caller — one convention on every page.

use leptos::prelude::*;
use serde::Serialize;
use sp_core::playback::{PlaybackMode, PlaybackState};
use sp_core::seek_model::{format_position, seek_target_ms};

use crate::api;
use crate::components::dub_mixer::DubMixer;
use crate::components::karaoke_mixer::KaraokeMixer;
use crate::components::preview_video::PreviewVideo;
use crate::store::DashboardStore;

#[derive(Serialize)]
struct SetModeBody {
    mode: String,
}

#[component]
pub fn Player(playlist_id: i64) -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");
    let pid = playlist_id;

    // --- now-playing derivations (each subscribes to store.now_playing) ---
    let np = move || store.now_playing.get().get(&pid).cloned();
    let has_content = move || np().map(|i| i.has_now_playing_content()).unwrap_or(false);
    let song = move || {
        np().map(|i| i.song)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Nič nehrá".to_string())
    };
    let artist = move || np().map(|i| i.artist).unwrap_or_default();
    let position = move || np().map(|i| i.position_ms).unwrap_or(0);
    let duration = move || np().map(|i| i.duration_ms).unwrap_or(0);
    let state = move || np().map(|i| i.state).unwrap_or_default();
    let mode = move || np().map(|i| i.mode).unwrap_or_default();

    let is_playing = Memo::new(move |_| matches!(state(), PlaybackState::Playing));

    // The pipeline is DECODING when it is Playing OR waiting off-program for its
    // scene with a real current video. Preparing a dub on the Dabing page before
    // cutting it in is exactly this off-program decoding state, so the preview +
    // mixer follow "is decoding", not "is on the program".
    let is_decoding = Memo::new(move |_| {
        matches!(
            state(),
            PlaybackState::Playing | PlaybackState::WaitingForScene
        ) && has_content()
    });

    // Playback-command errors (play / pause / skip / prev / seek / mode) surface
    // here as Slovak text and clear on the next successful command.
    let player_error = RwSignal::new(None::<String>);

    let state_label = move || match state() {
        PlaybackState::Playing => "Hrá",
        PlaybackState::Idle => "Nehrá",
        PlaybackState::WaitingForScene => "Čaká na scénu",
    };

    // On-program: the honest signal the store already has — the NDI-health
    // registry maps a Playing-but-off-program pipeline to `Paused`, so
    // `state == "Playing"` means the wall shows this output.
    let on_program = move || {
        store
            .ndi_health
            .get()
            .iter()
            .find(|o| o.playlist_id == pid)
            .map(|o| o.state == "Playing")
            .unwrap_or(false)
    };

    // --- transport (each command reports failure into `player_error`) ---
    let report = move |ctx: &'static str, r: Result<(), String>| match r {
        Ok(()) => player_error.set(None),
        Err(e) => player_error.set(Some(format!("{ctx}: {e}"))),
    };
    let do_play_pause = move |_| {
        let playing = is_playing.get_untracked();
        leptos::task::spawn_local(async move {
            let action = if playing { "pause" } else { "play" };
            let r = api::post_empty(&format!("/api/v1/playback/{pid}/{action}")).await;
            report(
                if playing {
                    "Pauza zlyhala"
                } else {
                    "Prehrávanie zlyhalo"
                },
                r,
            );
        });
    };
    let do_prev = move |_| {
        leptos::task::spawn_local(async move {
            let r = api::post_empty(&format!("/api/v1/playback/{pid}/previous")).await;
            report("Predošlá zlyhala", r);
        });
    };
    let do_skip = move |_| {
        leptos::task::spawn_local(async move {
            let r = api::post_empty(&format!("/api/v1/playback/{pid}/skip")).await;
            report("Ďalšia zlyhala", r);
        });
    };
    let on_mode = move |ev: leptos::ev::Event| {
        let val = event_target_value(&ev);
        let mode = PlaybackMode::from_str_lossy(&val);
        let body = SetModeBody {
            mode: mode.as_str().to_string(),
        };
        leptos::task::spawn_local(async move {
            let r = api::put_json_empty(&format!("/api/v1/playback/{pid}/mode"), &body).await;
            report("Zmena režimu zlyhala", r);
        });
    };

    // --- seek ---
    let do_seek = move |ms: u64| {
        leptos::task::spawn_local(async move {
            let r = api::seek_playlist(pid, ms).await;
            report("Pretáčanie zlyhalo", r);
        });
    };
    let seek_back = move |_| do_seek(seek_target_ms(position(), -10_000, duration()));
    let seek_fwd = move |_| do_seek(seek_target_ms(position(), 10_000, duration()));

    // --- preview (click-to-start; torn down when the pipeline stops decoding) ---
    let preview_on = RwSignal::new(false);
    Effect::new(move |_| {
        if !is_decoding.get() {
            preview_on.set(false);
        }
    });

    // --- mixer slot: chosen from the PLAYING item, collapsed to a Memo so the
    // frequent position ticks do NOT remount the mixer (only a change of the
    // playing video, or of its dub row, re-renders it). A dub row → the dub
    // adapter; otherwise the karaoke adapter for this playlist. `store.dabing`
    // is empty on non-Dabing pages, so those always get the karaoke mixer.
    let mixer_choice = Memo::new(move |_| {
        let vid = store.now_playing.get().get(&pid).map(|i| i.video_id);
        vid.and_then(|v| store.dabing.get().into_iter().find(|r| r.video_id == v))
    });

    view! {
        <div class="player" data-testid="player">
            <div class="player-head">
                <div class="player-titles">
                    <span class="player-title" data-testid="player-title">
                        {move || {
                            let a = artist();
                            if a.is_empty() {
                                song()
                            } else {
                                format!("{} — {}", song(), a)
                            }
                        }}
                    </span>
                    <span class="player-state" data-testid="player-state">
                        {state_label}
                    </span>
                </div>
                <span
                    class="player-program-badge"
                    class:on=on_program
                    data-testid="player-program-badge"
                >
                    {move || if on_program() { "● Na programe" } else { "○ Mimo programu" }}
                </span>
            </div>

            // --- error line: a failed command surfaces here, clears on success ---
            {move || {
                player_error
                    .get()
                    .map(|e| {
                        view! {
                            <div class="player-error" data-testid="player-error">
                                {e}
                            </div>
                        }
                    })
            }}

            // --- seek row (the range IS the progress; no second bar) ---
            <div class="player-seek-row">
                <input
                    type="range"
                    class="player-seek"
                    data-testid="player-seek"
                    min="0"
                    max=move || duration().to_string()
                    step="1000"
                    prop:value=move || position().to_string()
                    prop:disabled=move || !has_content()
                    on:change=move |ev| {
                        if let Ok(v) = event_target_value(&ev).parse::<u64>() {
                            do_seek(v);
                        }
                    }
                />
                <div class="player-seek-controls">
                    <button
                        type="button"
                        class="player-btn"
                        data-testid="player-back10"
                        title="Pretočiť o 10 s späť"
                        prop:disabled=move || !has_content()
                        on:click=seek_back
                    >
                        "−10 s"
                    </button>
                    <span class="player-pos" data-testid="player-pos">
                        {move || format!("{} / {}", format_position(position()), format_position(duration()))}
                    </span>
                    <button
                        type="button"
                        class="player-btn"
                        data-testid="player-fwd10"
                        title="Pretočiť o 10 s vpred"
                        prop:disabled=move || !has_content()
                        on:click=seek_fwd
                    >
                        "+10 s"
                    </button>
                </div>
            </div>

            // --- transport ---
            <div class="player-transport">
                <button
                    type="button"
                    class="player-btn"
                    data-testid="player-prev"
                    title="Predošlá"
                    on:click=do_prev
                >
                    "⏮ Predošlá"
                </button>
                <button
                    type="button"
                    class="player-btn player-btn-primary"
                    data-testid="player-playpause"
                    title=move || if is_playing.get() { "Pauza" } else { "Prehrať" }
                    on:click=do_play_pause
                >
                    {move || if is_playing.get() { "⏸ Pauza" } else { "▶ Prehrať" }}
                </button>
                <button
                    type="button"
                    class="player-btn"
                    data-testid="player-skip"
                    title="Ďalšia"
                    on:click=do_skip
                >
                    "⏭ Ďalšia"
                </button>
                <select
                    class="player-mode"
                    data-testid="player-mode"
                    title="Režim prehrávania"
                    prop:value=move || mode().as_str().to_string()
                    on:change=on_mode
                >
                    <option value="continuous">"Plynulo"</option>
                    <option value="single">"Jedna skladba"</option>
                    <option value="loop">"Opakovať"</option>
                </select>
            </div>

            // --- live A/V preview slot (click-to-start; available whenever the
            // pipeline is decoding, incl. an off-program dub on the Dabing page) ---
            <div class="player-preview">
                {move || {
                    if !is_decoding.get() {
                        view! {
                            <div class="preview-placeholder" data-testid="preview-placeholder">
                                "Bez náhľadu"
                            </div>
                        }
                            .into_any()
                    } else if preview_on.get() {
                        view! {
                            <PreviewVideo
                                playlist_id=pid
                                on_stop=Callback::new(move |_| preview_on.set(false))
                            />
                        }
                            .into_any()
                    } else {
                        view! {
                            <div class="preview-placeholder" data-testid="preview-placeholder">
                                <button
                                    type="button"
                                    class="preview-btn preview-start-btn"
                                    data-testid="preview-start"
                                    on:click=move |_| preview_on.set(true)
                                >
                                    "▶ Živý náhľad"
                                </button>
                            </div>
                        }
                            .into_any()
                    }
                }}
            </div>

            // --- mixer slot: collapses to one line when nothing plays; the
            // faders/presets appear only with a playing item, and the adapter
            // follows that item (dub row → dub mixer, else stems mixer) ---
            <div class="player-mixer">
                {move || {
                    if !has_content() {
                        view! {
                            <div class="player-mixer-idle" data-testid="player-mixer-idle">
                                "Mixér — nič nehrá"
                            </div>
                        }
                            .into_any()
                    } else {
                        match mixer_choice.get() {
                            Some(row) => {
                                view! {
                                    <DubMixer
                                        video_id=row.video_id
                                        title=row.title.clone()
                                        dub_status=row.dub_status.clone()
                                        dub_mix_ratio=row.dub_mix_ratio
                                        stem_status=row.stem_status.clone()
                                    />
                                }
                                    .into_any()
                            }
                            None => view! { <KaraokeMixer playlist_id=pid /> }.into_any(),
                        }
                    }
                }}
            </div>
        </div>
    }
}
