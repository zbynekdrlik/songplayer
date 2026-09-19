//! Card showing playlist info, now-playing, and playback controls.

use leptos::prelude::*;
use sp_core::models::Playlist;
use sp_core::playback::PlaybackState;

use crate::components::karaoke_panel;
use crate::components::ndi_health;
use crate::components::playback_controls;
use crate::components::preview_video;
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

    // #178: a stable memo drives whether this card is Playing. While Playing the
    // card offers a "▶ Živý náhľad" start control; CLICKING it mounts the live
    // A/V preview `<video>` (`preview_video::PreviewVideo`, which opens the
    // `preview.ws` stream on-demand). The #15 JPEG `<img>` + its ~3 fps poll loop
    // are gone — the stream is the single preview surface in the card (no dual
    // path). #178 round 3: the preview is CLICK-to-start, not auto-mounted — the
    // app's own hidden Tauri webview is a permanent viewer, so an auto-mounted
    // preview ran an encoder child 24/7 while anything played.
    let is_playing = Memo::new(move |_| {
        store.now_playing.with(|m| {
            m.get(&pid)
                .map(|i| matches!(i.state, PlaybackState::Playing))
                .unwrap_or(false)
        })
    });
    // Whether the operator has started the live preview for this card. Reset
    // whenever the card leaves the Playing state, so the stream (and its encoder
    // child) is always torn down when playback stops.
    let preview_on = RwSignal::new(false);
    Effect::new(move |_| {
        if !is_playing.get() {
            preview_on.set(false);
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
                // #178: the live A/V preview. While Playing the card shows a
                // start control; clicking it mounts the on-demand `preview.ws`
                // stream `<video>`. Not playing → the placeholder. All three
                // share the 16:9 box geometry (style.css) so the transitions
                // never shift layout (sp-ui-frontend.md).
                {move || {
                    if !is_playing.get() {
                        view! {
                            <div class="preview-placeholder" data-testid="preview-placeholder">
                                "Bez náhľadu"
                            </div>
                        }
                            .into_any()
                    } else if preview_on.get() {
                        view! {
                            <preview_video::PreviewVideo
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
