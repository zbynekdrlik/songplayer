//! Table of cached videos for a playlist, with a per-row "play this song
//! now" button (#134) — reuses the generic `POST /playlists/{id}/play-video`
//! endpoint that `api::post_live_play_video` already calls for the /live
//! setlist; nothing about that URL or payload is live-specific.

use leptos::prelude::*;
use sp_core::models::Video;

use crate::api;

#[component]
pub fn VideoList(playlist_id: i64) -> impl IntoView {
    let videos = RwSignal::new(Vec::<Video>::new());
    let error_msg = RwSignal::new(String::new());

    let pid = playlist_id;
    let load = move || {
        leptos::task::spawn_local(async move {
            let path = format!("/api/v1/playlists/{pid}/videos");
            if let Ok(v) = api::get::<Vec<Video>>(&path).await {
                videos.set(v);
            }
        });
    };
    let _load = Effect::new(move |_| load());

    view! {
        <div class="video-list">
            {move || {
                let err = error_msg.get();
                if err.is_empty() {
                    view! { <span></span> }.into_any()
                } else {
                    view! { <div class="video-list-error">{err}</div> }.into_any()
                }
            }}
            <table>
                <thead>
                    <tr>
                        <th class="video-list-col-play"></th>
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
                        children=move |video| {
                            let video_id = video.id;
                            let normalized = video.normalized;
                            let title = video.song.clone().unwrap_or_else(|| video.title.clone());
                            view! {
                                <tr>
                                    <td class="video-list-col-play">
                                        <button
                                            class="video-list-btn-play"
                                            title=if normalized {
                                                "Play this song now"
                                            } else {
                                                "Not yet processed — cannot play"
                                            }
                                            disabled=!normalized
                                            data-testid="video-list-play"
                                            on:click=move |_| {
                                                leptos::task::spawn_local(async move {
                                                    if let Err(e) = api::post_live_play_video(
                                                        pid, video_id, None,
                                                    )
                                                    .await
                                                    {
                                                        error_msg.set(e);
                                                    } else {
                                                        error_msg.set(String::new());
                                                    }
                                                });
                                            }
                                        >
                                            "▶"
                                        </button>
                                    </td>
                                    <td>{title}</td>
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
