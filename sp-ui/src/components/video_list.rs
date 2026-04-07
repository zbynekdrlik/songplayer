//! Table of cached videos for a playlist.

use leptos::prelude::*;
use sp_core::models::Video;

use crate::api;

#[component]
pub fn VideoList(playlist_id: i64) -> impl IntoView {
    let videos = RwSignal::new(Vec::<Video>::new());

    let pid = playlist_id;
    let _load = Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            let path = format!("/api/v1/playlists/{pid}/videos");
            if let Ok(v) = api::get::<Vec<Video>>(&path).await {
                videos.set(v);
            }
        });
    });

    view! {
        <div class="video-list">
            <table>
                <thead>
                    <tr>
                        <th>"Song"</th>
                        <th>"Artist"</th>
                        <th>"Cached"</th>
                        <th>"Normalized"</th>
                    </tr>
                </thead>
                <tbody>
                    <For
                        each=move || videos.get()
                        key=|v| v.id
                        children=|video| {
                            view! {
                                <tr>
                                    <td>{video.song.clone().unwrap_or_else(|| video.title.clone())}</td>
                                    <td>{video.artist.clone().unwrap_or_default()}</td>
                                    <td>{if video.cached { "Yes" } else { "No" }}</td>
                                    <td>{if video.normalized { "Yes" } else { "No" }}</td>
                                </tr>
                            }
                        }
                    />
                </tbody>
            </table>
        </div>
    }
}
