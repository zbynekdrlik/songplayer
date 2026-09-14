//! Card showing playlist info, now-playing, and playback controls.

use leptos::prelude::*;
use sp_core::models::Playlist;
use sp_core::playback::PlaybackState;

use crate::components::karaoke_panel;
use crate::components::ndi_health;
use crate::components::playback_controls;
use crate::components::video_list;
use crate::store::DashboardStore;

#[component]
pub fn PlaylistCard(playlist: Playlist) -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");
    let pid = playlist.id;
    // #150: this card's NDI output name, matched against the 1 Hz
    // `store.ndi_health` snapshot to render its genlock LockBadge.
    let ndi_name = playlist.ndi_output_name.clone();
    // #134: song list is collapsed by default — a busy playlist can have
    // dozens of cached videos, and most dashboard glances only care about
    // now-playing + transport controls. Toggled open on demand.
    let songs_open = RwSignal::new(false);

    // #15 part 2: live preview cache-buster. A ~3 fps tick drives the
    // `<img>` src's `?t=` so the browser re-fetches the latest sampled JPEG
    // from `GET /api/v1/playback/{id}/preview.jpg`. The image element only
    // exists while Playing (see below), so an idle card makes no requests.
    let preview_tick = RwSignal::new(0u64);
    // The <img> stays hidden (placeholder shown) until the first frame actually
    // loads, so a body-less 204 during warmup never flashes a broken image or
    // trips the zero-console-errors gate.
    let preview_loaded = RwSignal::new(false);
    let preview_cancelled = RwSignal::new(false);
    on_cleanup(move || preview_cancelled.set(true));
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            loop {
                // Page-owned signals: navigating away disposes them while the
                // task is parked in the timer — `try_*` (None on a disposed
                // signal) and stop, never panic (see sp-ui-frontend.md).
                if preview_cancelled.try_get_untracked() != Some(false) {
                    break;
                }
                gloo_timers::future::TimeoutFuture::new(300).await;
                if preview_tick
                    .try_update(|t| *t = t.wrapping_add(1))
                    .is_none()
                {
                    break;
                }
            }
        });
    });

    view! {
        <div class="playlist-card">
            <div class="card-header">
                <h3>{playlist.name.clone()}</h3>
                <span class="playlist-id">{playlist.ndi_output_name.clone()}</span>
                {
                    let ndi_name = ndi_name.clone();
                    move || {
                        store
                            .ndi_health
                            .get()
                            .into_iter()
                            .find(|o| o.ndi_name == ndi_name)
                            .map(|o| view! { <ndi_health::LockBadge output=o /> })
                    }
                }
            </div>

            <div class="now-playing">
                {move || {
                    let np = store.now_playing.get();
                    if let Some(info) = np.get(&pid) {
                        let pct = if info.duration_ms > 0 {
                            (info.position_ms as f64 / info.duration_ms as f64) * 100.0
                        } else {
                            0.0
                        };
                        let state_label = match info.state {
                            PlaybackState::Playing => "Playing",
                            PlaybackState::Idle => "Idle",
                            PlaybackState::WaitingForScene => "Waiting",
                        };
                        let pos_s = info.position_ms / 1000;
                        let dur_s = info.duration_ms / 1000;
                        let is_playing = matches!(info.state, PlaybackState::Playing);
                        view! {
                            <div>
                                // #15 part 2: live video preview of the current
                                // song. The <img> only exists while Playing, so
                                // an idle card issues no preview requests; a
                                // placeholder shows otherwise.
                                {move || {
                                    if is_playing {
                                        view! {
                                            <img
                                                class="preview-img"
                                                data-testid="preview-img"
                                                alt="Živý náhľad"
                                                style:display=move || {
                                                    if preview_loaded.get() { "block" } else { "none" }
                                                }
                                                on:load=move |_| preview_loaded.set(true)
                                                on:error=move |_| preview_loaded.set(false)
                                                src=move || {
                                                    format!(
                                                        "/api/v1/playback/{pid}/preview.jpg?t={}",
                                                        preview_tick.get(),
                                                    )
                                                }
                                            />
                                            {move || {
                                                (!preview_loaded.get())
                                                    .then(|| {
                                                        view! {
                                                            <div
                                                                class="preview-placeholder"
                                                                data-testid="preview-placeholder"
                                                            >
                                                                "Načítavam náhľad…"
                                                            </div>
                                                        }
                                                    })
                                            }}
                                        }
                                            .into_any()
                                    } else {
                                        view! {
                                            <div
                                                class="preview-placeholder"
                                                data-testid="preview-placeholder"
                                            >
                                                "Bez náhľadu"
                                            </div>
                                        }
                                            .into_any()
                                    }
                                }}
                                <div class="np-info">
                                    <span class="np-song">{info.song.clone()}</span>
                                    <span class="np-artist">{info.artist.clone()}</span>
                                    <div class="progress-bar">
                                        <div
                                            class="progress-fill"
                                            style:width=format!("{pct:.1}%")
                                        ></div>
                                    </div>
                                    <span class="np-time">
                                        {format!(
                                            "{}:{:02} / {}:{:02}  [{}]  {}",
                                            pos_s / 60,
                                            pos_s % 60,
                                            dur_s / 60,
                                            dur_s % 60,
                                            state_label,
                                            info.mode.as_str(),
                                        )}
                                    </span>
                                </div>
                                <karaoke_panel::KaraokePanel info=info.clone() />
                            </div>
                        }
                            .into_any()
                    } else {
                        view! { <p class="np-idle">"Nothing playing"</p> }.into_any()
                    }
                }}
            </div>

            <playback_controls::PlaybackControls playlist_id=pid />

            <div class="playlist-songs">
                <button
                    class="playlist-songs-toggle"
                    data-testid="playlist-songs-toggle"
                    on:click=move |_| songs_open.update(|o| *o = !*o)
                >
                    {move || if songs_open.get() { "▼ Songs" } else { "▶ Songs" }}
                </button>
                {move || {
                    if songs_open.get() {
                        view! { <video_list::VideoList playlist_id=pid /> }.into_any()
                    } else {
                        view! { <span></span> }.into_any()
                    }
                }}
            </div>
        </div>
    }
}
