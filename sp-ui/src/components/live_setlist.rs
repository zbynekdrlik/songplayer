//! /live set list. Uses the shared `SongRow` + `StatusChips` (#194): each row is
//! title/artist + the text chip + the primary play action, with the set-list
//! actions (reorder, EN-suppress, remove) in the actions slot.

use leptos::prelude::*;
use sp_core::status_chip::text_chip;

use crate::api;
use crate::components::song_row::SongRow;
use crate::components::status_chips::ChipView;

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
            <h2>"Zoznam skladieb — ytlive"</h2>
            <div class="live-setlist-error">{move || error_msg.get()}</div>
            <div class="song-list">
                <For
                    each=enriched
                    key=|(it, meta)| {
                        (
                            it["video_id"].as_i64().unwrap_or(0),
                            it["position"].as_i64().unwrap_or(0),
                            meta["has_lyrics"].as_bool().unwrap_or(false),
                            meta["is_stale"].as_bool().unwrap_or(false),
                            meta["lyrics_reference"].as_bool().unwrap_or(false),
                        )
                    }
                    children=move |(item, meta)| {
                        let position = item["position"].as_i64().unwrap_or(0);
                        let video_id = item["video_id"].as_i64().unwrap_or(0);
                        let song = meta["song"].as_str().unwrap_or("—").to_string();
                        let artist = meta["artist"].as_str().unwrap_or_default().to_string();
                        let song_for_confirm = song.clone();
                        let title = format!("{}. {}", position + 1, song);
                        let suppress_initial = meta["suppress_resolume_en"]
                            .as_bool()
                            .unwrap_or(false);
                        let has_lyrics = meta["has_lyrics"].as_bool().unwrap_or(false);
                        let is_stale = meta["is_stale"].as_bool().unwrap_or(false);
                        let is_ref = meta["lyrics_reference"].as_bool().unwrap_or(false);
                        let chips = vec![ChipView::new(text_chip(has_lyrics, is_ref, is_stale))];
                        let on_play = Callback::new(move |_| {
                            leptos::task::spawn_local(async move {
                                if let Err(e) = api::post_live_play_video(playlist_id, video_id, None)
                                    .await
                                {
                                    error_msg.set(e);
                                }
                            });
                        });
                        view! {
                            <SongRow
                                video_id=video_id
                                title=title
                                artist=artist
                                chips=chips
                                on_play=on_play
                                play_ready=true
                            >
                                <button
                                    type="button"
                                    class="song-row-btn"
                                    title="Posunúť vyššie"
                                    on:click=move |_| {
                                        leptos::task::spawn_local(async move {
                                            match api::post_live_move_item(playlist_id, video_id, "up")
                                                .await
                                            {
                                                Ok(()) => on_changed.run(()),
                                                Err(e) => error_msg.set(e),
                                            }
                                        });
                                    }
                                >
                                    "▲"
                                </button>
                                <button
                                    type="button"
                                    class="song-row-btn"
                                    title="Posunúť nižšie"
                                    on:click=move |_| {
                                        leptos::task::spawn_local(async move {
                                            match api::post_live_move_item(playlist_id, video_id, "down")
                                                .await
                                            {
                                                Ok(()) => on_changed.run(()),
                                                Err(e) => error_msg.set(e),
                                            }
                                        });
                                    }
                                >
                                    "▼"
                                </button>
                                <label
                                    class="song-row-enoff"
                                    title="Nevysielať anglický riadok textu do Resolume #sp-subs klipov"
                                >
                                    <input
                                        type="checkbox"
                                        prop:checked=suppress_initial
                                        on:change=move |ev| {
                                            let checked = event_target_checked(&ev);
                                            leptos::task::spawn_local(async move {
                                                match api::patch_video_suppress_en(video_id, checked)
                                                    .await
                                                {
                                                    Ok(()) => on_changed.run(()),
                                                    Err(e) => error_msg.set(e),
                                                }
                                            });
                                        }
                                    />
                                    "EN"
                                </label>
                                <button
                                    type="button"
                                    class="song-row-btn song-row-btn-remove"
                                    title="Odobrať zo set listu"
                                    on:click=move |_| {
                                        let ok = web_sys::window()
                                            .and_then(|w| {
                                                w.confirm_with_message(
                                                        &format!(
                                                            "Odobrať \"{song_for_confirm}\" zo set listu?",
                                                        ),
                                                    )
                                                    .ok()
                                            })
                                            .unwrap_or(false);
                                        if !ok {
                                            return;
                                        }
                                        leptos::task::spawn_local(async move {
                                            match api::delete_live_item(playlist_id, video_id).await {
                                                Ok(()) => on_changed.run(()),
                                                Err(e) => error_msg.set(e),
                                            }
                                        });
                                    }
                                >
                                    "✕"
                                </button>
                            </SongRow>
                        }
                    }
                />
            </div>
        </div>
    }
}
