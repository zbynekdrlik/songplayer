//! #194: a thin playlist card = playlist header (name + NDI + genlock badge) +
//! the shared `Player` + the song list. The now-playing block, the live preview,
//! and the playback controls that used to live here are now the `Player`'s job
//! (one playback surface everywhere). The 4-line karaoke/lyrics panel stays for
//! now — it is a lyrics-preview surface unified separately in a later round.

use leptos::prelude::*;
use sp_core::models::Playlist;

use crate::components::karaoke_panel;
use crate::components::ndi_health;
use crate::components::player;
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

    // The 4-line lyrics panel (a lyrics-preview surface, not a playback control)
    // still reads the selected playlist's now-playing entry.
    let np_info = move || store.now_playing.get().get(&pid).cloned();

    view! {
        <div class="playlist-card">
            <div class="card-header">
                <h3 data-testid="workspace-title">{playlist.name.clone()}</h3>
                <span class="playlist-id">{playlist.ndi_output_name.clone()}</span>
                {
                    let ndi_name = ndi_name.clone();
                    move || {
                        // #164: show the badge only on live pacing-enabled
                        // outputs. #165: and only when `show_badge` (off on the
                        // work area — the badge lives in the selector rows +
                        // header summary instead).
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

            // #194: the ONE playback surface (now-playing, badge, seek, transport,
            // mode, preview, mixer) — identical to Live and Dabing.
            <player::Player playlist_id=pid />

            // Lyrics preview (unified in a later round; kept here so the dashboard
            // does not lose its subtitle glance).
            {move || match np_info() {
                Some(info) if info.has_now_playing_content() => {
                    view! { <karaoke_panel::KaraokePanel info=info /> }.into_any()
                }
                _ => view! { <span></span> }.into_any(),
            }}

            <div class="playlist-songs">
                <button
                    class="playlist-songs-toggle"
                    data-testid="playlist-songs-toggle"
                    on:click=move |_| songs_open.update(|o| *o = !*o)
                >
                    {move || if songs_open.get() { "▼ Skladby" } else { "▶ Skladby" }}
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
