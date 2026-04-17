//! Right pane of /live: the current set list with ▶ / ✕ buttons per row
//! and the standard playback controls bound to the custom playlist id.

use leptos::prelude::*;

use crate::api;

#[component]
pub fn LiveSetList(
    playlist_id: i64,
    #[prop(into)] refresh: Signal<u64>,
    on_changed: Callback<()>,
) -> impl IntoView {
    let items = RwSignal::new(Vec::<serde_json::Value>::new());
    let songs = RwSignal::new(Vec::<serde_json::Value>::new());
    let error_msg = RwSignal::new(String::new());

    // Reload whenever `refresh` bumps (add/remove/initial mount).
    let _load = Effect::new(move |_| {
        let _tick = refresh.get();
        leptos::task::spawn_local(async move {
            let items_res = api::get_live_items(playlist_id).await;
            let songs_res = api::get_lyrics_songs(None).await;
            match (items_res, songs_res) {
                (Ok(i), Ok(s)) => {
                    items.set(i);
                    songs.set(s);
                }
                (Err(e), _) | (_, Err(e)) => error_msg.set(e),
            }
        });
    });

    let enriched = move || {
        let idx: std::collections::HashMap<i64, serde_json::Value> = songs
            .get()
            .into_iter()
            .filter_map(|s| s["video_id"].as_i64().map(|id| (id, s)))
            .collect();
        items
            .get()
            .into_iter()
            .map(|it| {
                let video_id = it["video_id"].as_i64().unwrap_or(0);
                let meta = idx.get(&video_id).cloned().unwrap_or_default();
                (it, meta)
            })
            .collect::<Vec<_>>()
    };

    view! {
        <div class="live-setlist">
            <h2>"ytlive set list"</h2>
            <div class="live-setlist-error">{move || error_msg.get()}</div>
            <table class="live-setlist-table">
                <thead>
                    <tr>
                        <th>"#"</th>
                        <th>"Song"</th>
                        <th>"Artist"</th>
                        <th></th>
                    </tr>
                </thead>
                <tbody>
                    <For
                        each=enriched
                        key=|(it, _)| it["video_id"].as_i64().unwrap_or(0)
                        children=move |(item, meta)| {
                            let position = item["position"].as_i64().unwrap_or(0);
                            let video_id = item["video_id"].as_i64().unwrap_or(0);
                            let song = meta["song"].as_str().unwrap_or("—").to_string();
                            let artist = meta["artist"].as_str().unwrap_or("—").to_string();
                            view! {
                                <tr>
                                    <td>{position + 1}</td>
                                    <td>{song}</td>
                                    <td>{artist}</td>
                                    <td>
                                        <button on:click=move |_| {
                                            leptos::task::spawn_local(async move {
                                                if let Err(e) = api::post_live_play_video(
                                                    playlist_id, video_id,
                                                ).await {
                                                    error_msg.set(e);
                                                }
                                            });
                                        }>"▶"</button>
                                        <button on:click=move |_| {
                                            leptos::task::spawn_local(async move {
                                                match api::delete_live_item(
                                                    playlist_id, video_id,
                                                ).await {
                                                    Ok(()) => on_changed.run(()),
                                                    Err(e) => error_msg.set(e),
                                                }
                                            });
                                        }>"✕"</button>
                                    </td>
                                </tr>
                            }
                        }
                    />
                </tbody>
            </table>
            <div class="live-setlist-controls">
                <button on:click=move |_| {
                    leptos::task::spawn_local(async move {
                        let _ = api::post_empty(
                            &format!("/api/v1/playback/{playlist_id}/play"),
                        ).await;
                    });
                }>"▶ Play"</button>
                <button on:click=move |_| {
                    leptos::task::spawn_local(async move {
                        let _ = api::post_empty(
                            &format!("/api/v1/playback/{playlist_id}/pause"),
                        ).await;
                    });
                }>"⏸"</button>
                <button on:click=move |_| {
                    leptos::task::spawn_local(async move {
                        let _ = api::post_empty(
                            &format!("/api/v1/playback/{playlist_id}/skip"),
                        ).await;
                    });
                }>"⏭"</button>
                <button on:click=move |_| {
                    leptos::task::spawn_local(async move {
                        let _ = api::post_empty(
                            &format!("/api/v1/playback/{playlist_id}/previous"),
                        ).await;
                    });
                }>"⏮"</button>
            </div>
        </div>
    }
}
