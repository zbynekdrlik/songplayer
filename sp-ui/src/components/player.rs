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
use sp_core::mixer_model::mixer_controls;
use sp_core::playback::{PlaybackMode, PlaybackState, TransportState};
use sp_core::preview_lag::preview_lag_display;
use sp_core::seek_model::{format_position, seek_display_ms, seek_target_ms};

use crate::api;
use crate::components::dub_mixer::DubMixer;
use crate::components::karaoke_mixer::KaraokeMixer;
use crate::components::lyrics_view::LyricsView;
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
    // #194 hotfix: a `Memo`, not a plain closure. A plain closure re-subscribes
    // to `store.now_playing` and re-runs on EVERY position tick, so any slot
    // closure that read `has_content()` (the mixer slot) re-created its child
    // twice a second — resetting a mid-drag fader. A `Memo` only propagates when
    // the boolean actually flips, so a position tick no longer touches the slot.
    let has_content = Memo::new(move |_| np().map(|i| i.has_now_playing_content()).unwrap_or(false));
    let song = move || {
        np().map(|i| i.song)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Nič nehrá".to_string())
    };
    let artist = move || np().map(|i| i.artist).unwrap_or_default();
    let position = move || np().map(|i| i.position_ms).unwrap_or(0);
    let duration = move || np().map(|i| i.duration_ms).unwrap_or(0);
    let state = move || np().map(|i| i.state).unwrap_or_default();
    let transport = move || np().map(|i| i.transport).unwrap_or_default();
    let mode = move || np().map(|i| i.mode).unwrap_or_default();

    // #201: the play/pause label follows the pipeline's own TRANSPORT state, not
    // the scene-aware `state` — a dub decoding OFF program (state
    // WaitingForScene, transport Playing) reads `⏸ Pauza` and a click posts
    // /pause. On/off-program shows only in the badge (from `ndi_health`).
    let is_playing = Memo::new(move |_| matches!(transport(), TransportState::Playing));

    // The pipeline is DECODING when it is Playing OR waiting off-program for its
    // scene with a real current video. Preparing a dub on the Dabing page before
    // cutting it in is exactly this off-program decoding state, so the preview +
    // mixer follow "is decoding", not "is on the program".
    let is_decoding = Memo::new(move |_| {
        matches!(
            state(),
            PlaybackState::Playing | PlaybackState::WaitingForScene
        ) && has_content.get()
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
    // `state == "Playing"` means the wall shows this output. #194 hotfix: a
    // `Memo` so the 1 Hz health poll only re-renders the badge when it flips.
    let on_program = Memo::new(move |_| {
        store
            .ndi_health
            .get()
            .iter()
            .find(|o| o.playlist_id == pid)
            .map(|o| o.state == "Playing")
            .unwrap_or(false)
    });

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

    // --- seek (with a drag gate) ---
    // While the pointer is down the DRAGGED value is authoritative, so the
    // twice-a-second position tick can't snap the thumb back mid-drag; exactly
    // ONE seek POST fires on release (`on:change`). #194 hotfix.
    let seek_dragging = RwSignal::new(false);
    let seek_drag_ms = RwSignal::new(0_u64);
    // #200: the commit happens on RELEASE (`pointerup`/`touchend`) from the pending
    // drag value — a real browser fires `pointerup` before `change`, and once the
    // gate re-applies the live value Chrome suppresses `change` entirely, so
    // `change` is only the keyboard path. Value-dedup keeps it to ONE POST.
    let seek_committed = RwSignal::new(None::<u64>);
    // #198 item 1: a `dirty` latch set by `on:input`, cleared on commit. A bare
    // `change` with no preceding `input` in this session (a programmatic / stale
    // dispatch, or a keyboard change that never moved the slider) must NOT commit
    // the initial 0 ms — `seek_drag_ms` starts at 0. `on:change` reads this latch
    // and is a no-op when it is false.
    let seek_dirty = RwSignal::new(false);
    let do_seek = move |ms: u64| {
        leptos::task::spawn_local(async move {
            let r = api::seek_playlist(pid, ms).await;
            report("Pretáčanie zlyhalo", r);
        });
    };
    let commit_seek = move |ms: u64| {
        seek_dirty.set(false);
        if seek_committed.get_untracked() != Some(ms) {
            seek_committed.set(Some(ms));
            do_seek(ms);
        }
    };
    let seek_back = move |_| do_seek(seek_target_ms(position(), -10_000, duration()));
    let seek_fwd = move |_| do_seek(seek_target_ms(position(), 10_000, duration()));

    // --- preview (click-to-start; torn down when the pipeline stops decoding) ---
    let preview_on = RwSignal::new(false);
    // #184 round F: the latest picture lag (seconds behind the wall) the preview
    // shim reported over the 1 Hz beacon; the readout shows only at ≥ 3 s.
    let preview_lag = RwSignal::new(0.0_f64);
    // Fold the raw shim reports (which arrive ~4×/s from the pump tick) into the
    // DISPLAYED readout (Option<i64>). A Memo only propagates when that value
    // changes, so the span re-renders on a real change, not on every tick — the
    // shared sp-ui rule that a slot closure reads a Memo, not a chatty signal.
    let preview_lag_readout = Memo::new(move |_| {
        if preview_on.get() {
            preview_lag_display(preview_lag.get())
        } else {
            None
        }
    });
    Effect::new(move |_| {
        if !is_decoding.get() {
            preview_on.set(false);
        }
    });

    // --- mixer slot: chosen from the PLAYING item, collapsed to a Memo so the
    // frequent position ticks do NOT remount the mixer (only a change of the
    // playing video, or of its dub row, re-renders it). A dub row → the dub
    // adapter; a stems-capable song → the karaoke adapter; BOTH when both apply
    // (#184 B2). `store.dabing` is now app-polled, so the dub adapter appears on
    // every page — Dashboard / Live too, not only after visiting /dabing.
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
                    class:on=move || on_program.get()
                    data-testid="player-program-badge"
                >
                    {move || if on_program.get() { "● Na programe" } else { "○ Mimo programu" }}
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
                    prop:value=move || {
                        seek_display_ms(seek_dragging.get(), seek_drag_ms.get(), position())
                            .to_string()
                    }
                    prop:disabled=move || !has_content.get()
                    on:pointerdown=move |_| {
                        seek_committed.set(None); // a new drag may land on the old value
                        seek_drag_ms.set(position());
                        seek_dragging.set(true);
                    }
                    on:touchstart=move |_| {
                        seek_committed.set(None);
                        seek_drag_ms.set(position());
                        seek_dragging.set(true);
                    }
                    on:input=move |ev| {
                        if let Ok(v) = event_target_value(&ev).parse::<u64>() {
                            seek_drag_ms.set(v);
                            seek_dirty.set(true);
                        }
                    }
                    on:change=move |_| {
                        // Keyboard / programmatic path (a pointer release already
                        // committed and the dedup makes this a no-op then). #198:
                        // a bare `change` with no preceding `input` this session is
                        // a no-op — the `dirty` latch gates it, so a stale/synthetic
                        // change never commits the initial 0 ms.
                        if seek_dirty.get_untracked() {
                            commit_seek(seek_drag_ms.get_untracked());
                        }
                        seek_dragging.set(false);
                    }
                    on:pointerup=move |_| {
                        if seek_dragging.get_untracked() {
                            commit_seek(seek_drag_ms.get_untracked());
                        }
                        seek_dragging.set(false);
                    }
                    on:touchend=move |_| {
                        if seek_dragging.get_untracked() {
                            commit_seek(seek_drag_ms.get_untracked());
                        }
                        seek_dragging.set(false);
                    }
                    on:pointercancel=move |_| seek_dragging.set(false)
                />
                <div class="player-seek-controls">
                    <button
                        type="button"
                        class="player-btn"
                        data-testid="player-back10"
                        title="Pretočiť o 10 s späť"
                        prop:disabled=move || !has_content.get()
                        on:click=seek_back
                    >
                        "−10 s"
                    </button>
                    <span class="player-pos" data-testid="player-pos">
                        {move || {
                            let p = seek_display_ms(
                                seek_dragging.get(),
                                seek_drag_ms.get(),
                                position(),
                            );
                            format!("{} / {}", format_position(p), format_position(duration()))
                        }}
                    </span>
                    <button
                        type="button"
                        class="player-btn"
                        data-testid="player-fwd10"
                        title="Pretočiť o 10 s vpred"
                        prop:disabled=move || !has_content.get()
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
                                on_lag=Callback::new(move |s: f64| preview_lag.set(s))
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
                                    on:click=move |_| {
                                        preview_lag.set(0.0);
                                        preview_on.set(true);
                                    }
                                >
                                    "▶ Živý náhľad"
                                </button>
                            </div>
                        }
                            .into_any()
                    }
                }}
                // #184 round F: the lag readout — shown only while the preview is
                // mounted AND the picture is ≥ 3 s behind the wall (the pure
                // sp_core threshold), so the owner sees at a glance that the
                // PICTURE is late, not the control he just moved.
                {move || {
                    preview_lag_readout
                        .get()
                        .map(|n| {
                            view! {
                                <span class="preview-lag" data-testid="preview-lag">
                                    {format!("náhľad mešká {n} s")}
                                </span>
                            }
                        })
                }}
            </div>

            // --- mixer slot: collapses to one line when nothing plays; the
            // faders/presets appear only with a playing item, and the adapter
            // follows that item (dub row → dub mixer, else stems mixer) ---
            <div class="player-mixer">
                {move || {
                    if !has_content.get() {
                        view! {
                            <div class="player-mixer-idle" data-testid="player-mixer-idle">
                                "Mixér — nič nehrá"
                            </div>
                        }
                            .into_any()
                    } else {
                        // #184 B2: render the dub mixer and/or the karaoke mixer
                        // from the pure predicate. A dub video shows the dub
                        // mixer; the karaoke mixer shows for a stems-capable dub
                        // AND for any non-dub song (the plain-song default —
                        // KaraokeMixer self-locks when the song has no stems).
                        let choice = mixer_choice.get();
                        let controls = mixer_controls(
                            choice.as_ref().map(|r| r.dub_status.as_str()),
                            choice.as_ref().and_then(|r| r.stem_status.as_deref()),
                        );
                        let show_karaoke = controls.karaoke || !controls.dub;
                        let dub_panel = choice
                            .filter(|_| controls.dub)
                            .map(|row| {
                                view! {
                                    <DubMixer
                                        video_id=row.video_id
                                        title=row.title
                                        dub_status=row.dub_status
                                        dub_mix_ratio=row.dub_mix_ratio
                                        stem_status=row.stem_status
                                    />
                                }
                            });
                        view! {
                            {dub_panel}
                            {show_karaoke.then(|| view! { <KaraokeMixer playlist_id=pid /> })}
                        }
                            .into_any()
                    }
                }}
            </div>

            // --- lyrics slot: the ONE shared LyricsView (compact karaoke
            // preview) — identical on Dashboard, Live and Dabing; the dub's
            // subtitles arrive over the same now-playing WS lines (#194 r3c).
            <div class="player-lyrics">
                <LyricsView playlist_id=pid />
            </div>
        </div>
    }
}
