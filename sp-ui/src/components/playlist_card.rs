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
pub fn PlaylistCard(
    playlist: Playlist,
    // #165: the genlock badge (#164) belongs in the selector rows + header
    // summary, NOT on the single work area. The workspace renders the card
    // with `show_badge=false`; the default keeps the badge for any other use.
    #[prop(default = true)] show_badge: bool,
) -> impl IntoView {
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

    // #15 part 2 (recursive-closure fix): the preview <img> is created ONCE,
    // outside the now-playing rebuild block, so a position update (~2 Hz) never
    // drops the element together with its on:load/on:error closures while a
    // fetch is in flight (the "closure invoked recursively or after being
    // dropped" console error). A stable memo drives whether this card is Playing.
    let is_playing = Memo::new(move |_| {
        store.now_playing.with(|m| {
            m.get(&pid)
                .map(|i| matches!(i.state, PlaybackState::Playing))
                .unwrap_or(false)
        })
    });
    // While idle the <img> points at a 1×1 transparent GIF data-URI — no network
    // request, no error event, and the tick is not tracked.
    const PREVIEW_IDLE_SRC: &str =
        "data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";
    // Reset the loaded flag when playback stops so the placeholder returns.
    Effect::new(move |_| {
        if !is_playing.get() {
            preview_loaded.set(false);
        }
    });

    view! {
        <div class="playlist-card">
            <div class="card-header">
                <h3 data-testid="workspace-title">{playlist.name.clone()}</h3>
                <span class="playlist-id">{playlist.ndi_output_name.clone()}</span>
                {
                    let ndi_name = ndi_name.clone();
                    move || {
                        // #164: show the badge only on live pacing-enabled
                        // outputs — no '● UNLOCKED — pacing disabled' noise on
                        // every card while pacing is off. #165: and only when
                        // `show_badge` (off on the work area — the badge lives
                        // in the selector rows + header summary instead).
                        show_badge
                            .then(|| {
                                store
                                    .ndi_health
                                    .get()
                                    .into_iter()
                                    .find(|o| o.ndi_name == ndi_name)
                                    .filter(ndi_health::should_show_lock_badge)
                                    .map(|o| view! { <ndi_health::LockBadge output=o /> })
                            })
                            .flatten()
                    }
                }
            </div>

            <div class="now-playing">
                // #15 part 2: ONE stable preview <img> per card, created once and
                // never rebuilt. While idle its src is an inline data-URI (no
                // network request); while Playing the tick-driven cache-buster
                // re-fetches the sampled JPEG. Kept outside the now-playing
                // rebuild block so a position update never drops it mid-fetch.
                <img
                    class="preview-img"
                    data-testid="preview-img"
                    alt="Živý náhľad"
                    style:display=move || {
                        if is_playing.get() && preview_loaded.get() { "block" } else { "none" }
                    }
                    on:load=move |_| preview_loaded.set(true)
                    on:error=move |_| preview_loaded.set(false)
                    src=move || {
                        if is_playing.get() {
                            format!("/api/v1/playback/{pid}/preview.jpg?t={}", preview_tick.get())
                        } else {
                            PREVIEW_IDLE_SRC.to_string()
                        }
                    }
                />
                {move || {
                    (!(is_playing.get() && preview_loaded.get()))
                        .then(|| {
                            view! {
                                <div class="preview-placeholder" data-testid="preview-placeholder">
                                    {if is_playing.get() { "Načítavam náhľad…" } else { "Bez náhľadu" }}
                                </div>
                            }
                        })
                }}
                {move || {
                    let np = store.now_playing.get();
                    // #170: a PlaybackStateChanged-only entry (empty song, zero
                    // duration) is not real playback — render the idle state,
                    // never a bogus "0:00 / 0:00" np-info block.
                    if let Some(info) = np.get(&pid).filter(|i| i.has_now_playing_content()) {
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
                        view! {
                            <div>
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
