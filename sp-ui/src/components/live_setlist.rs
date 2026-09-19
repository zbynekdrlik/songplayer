//! Primary control surface of `/live`: the current set list with per-row
//! actions. #194: the global transport/mode bar that used to live here is gone —
//! the shared `Player` (rendered by the Live page under the setlist) is the ONE
//! playback surface. Per-row ▶ still plays a specific song from zero.

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
                // Actions cell first (after #) so the primary ▶ / ✕ buttons
                // stay pinned on the left edge of the screen and never overflow
                // on narrow phones. EN-off + reorder arrows move to a
                // secondary column that may scroll off on very narrow widths
                // — losing them is cheap, losing ▶ is not.
                <thead>
                    <tr>
                        <th>"#"</th>
                        <th class="live-setlist-col-play">""</th>
                        <th>"Song"</th>
                        <th class="live-setlist-col-secondary"></th>
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
                            let song_for_confirm = song.clone();
                            let suppress_initial = meta["suppress_resolume_en"]
                                .as_bool()
                                .unwrap_or(false);
                            view! {
                                <tr>
                                    <td>{position + 1}</td>
                                    <td class="live-setlist-col-play">
                                        <button
                                            class="live-setlist-btn live-setlist-btn-play"
                                            title="Play this song"
                                            on:click=move |_| {
                                                // Per-row play starts a specific song from zero.
                                                leptos::task::spawn_local(async move {
                                                    if let Err(e) = api::post_live_play_video(
                                                        playlist_id, video_id, None,
                                                    )
                                                    .await
                                                    {
                                                        error_msg.set(e);
                                                    }
                                                });
                                            }
                                        >"▶"</button>
                                    </td>
                                    <td class="live-setlist-song">{song}</td>
                                    <td class="live-setlist-col-secondary live-setlist-secondary">
                                        <button
                                            class="live-setlist-btn live-setlist-btn-move"
                                            title="Move up"
                                            on:click=move |_| {
                                                leptos::task::spawn_local(async move {
                                                    match api::post_live_move_item(
                                                        playlist_id, video_id, "up",
                                                    ).await {
                                                        Ok(()) => on_changed.run(()),
                                                        Err(e) => error_msg.set(e),
                                                    }
                                                });
                                            }
                                        >"▲"</button>
                                        <button
                                            class="live-setlist-btn live-setlist-btn-move"
                                            title="Move down"
                                            on:click=move |_| {
                                                leptos::task::spawn_local(async move {
                                                    match api::post_live_move_item(
                                                        playlist_id, video_id, "down",
                                                    ).await {
                                                        Ok(()) => on_changed.run(()),
                                                        Err(e) => error_msg.set(e),
                                                    }
                                                });
                                            }
                                        >"▼"</button>
                                        <label
                                            class="live-setlist-enoff-inline"
                                            title="Suppress pushing the English lyric line to Resolume #sp-subs clips"
                                        >
                                            <input
                                                type="checkbox"
                                                prop:checked=suppress_initial
                                                on:change=move |ev| {
                                                    let checked = event_target_checked(&ev);
                                                    leptos::task::spawn_local(async move {
                                                        match api::patch_video_suppress_en(
                                                            video_id, checked,
                                                        ).await {
                                                            Ok(()) => on_changed.run(()),
                                                            Err(e) => error_msg.set(e),
                                                        }
                                                    });
                                                }
                                            />
                                            "EN"
                                        </label>
                                        <button
                                            class="live-setlist-btn live-setlist-btn-remove"
                                            title="Remove from set list"
                                            on:click=move |_| {
                                                // Confirm so a stray tap during a live set doesn't
                                                // silently drop a song the band still needs.
                                                let ok = web_sys::window()
                                                    .and_then(|w| w.confirm_with_message(
                                                        &format!("Remove \"{song_for_confirm}\" from the set list?"),
                                                    ).ok())
                                                    .unwrap_or(false);
                                                if !ok { return; }
                                                leptos::task::spawn_local(async move {
                                                    match api::delete_live_item(
                                                        playlist_id, video_id,
                                                    ).await {
                                                        Ok(()) => on_changed.run(()),
                                                        Err(e) => error_msg.set(e),
                                                    }
                                                });
                                            }
                                        >"✕"</button>
                                    </td>
                                </tr>
                            }
                        }
                    />
                </tbody>
            </table>
        </div>
    }
}
