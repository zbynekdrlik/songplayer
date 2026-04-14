//! Card showing playlist info, now-playing, and playback controls.

use leptos::prelude::*;
use sp_core::models::Playlist;
use sp_core::playback::PlaybackState;

use crate::components::karaoke_panel;
use crate::components::playback_controls;
use crate::store::DashboardStore;

#[component]
pub fn PlaylistCard(playlist: Playlist) -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");
    let pid = playlist.id;

    view! {
        <div class="playlist-card">
            <div class="card-header">
                <h3>{playlist.name.clone()}</h3>
                <span class="playlist-id">{playlist.ndi_output_name.clone()}</span>
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
        </div>
    }
}
