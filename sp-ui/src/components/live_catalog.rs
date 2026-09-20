//! Left pane of /live: lists all songs from the catalog with an optional
//! "len s textom" filter and a "+ Pridať" action per row that appends the song
//! to the given custom playlist's set list. Uses the shared `SongRow` +
//! `StatusChips` (#194) so a catalog song looks the same as everywhere else.

use leptos::prelude::*;
use sp_core::status_chip::text_chip;

use crate::api;
use crate::components::song_row::SongRow;
use crate::components::status_chips::ChipView;

#[component]
pub fn LiveCatalog(
    /// The custom playlist that add-clicks target.
    target_playlist_id: i64,
    /// Bumped by the parent whenever the set list changes; the catalog
    /// currently ignores it but the signal is carried so future changes
    /// (e.g. per-row "already-added" badges) can observe the edit.
    #[prop(into)]
    _set_list_version: Signal<u64>,
    /// Callback fired with the video_id after a successful add. Lets the
    /// parent refresh the set-list view.
    on_added: Callback<i64>,
) -> impl IntoView {
    let songs = RwSignal::new(Vec::<serde_json::Value>::new());
    let show_only_with_lyrics = RwSignal::new(true);
    let error_msg = RwSignal::new(String::new());

    // Load the full catalog on mount.
    let _load = Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            match api::get_lyrics_songs(None).await {
                Ok(list) => songs.set(list),
                Err(e) => error_msg.set(format!("načítanie skladieb zlyhalo: {e}")),
            }
        });
    });

    let visible = move || {
        let all = songs.get();
        let filter = show_only_with_lyrics.get();
        all.into_iter()
            .filter(|s| {
                if filter {
                    s["has_lyrics"].as_bool().unwrap_or(false)
                } else {
                    true
                }
            })
            .collect::<Vec<_>>()
    };

    view! {
        <div class="live-catalog">
            <div class="live-catalog-header">
                <h2>"Katalóg"</h2>
                <label>
                    <input
                        type="checkbox"
                        prop:checked=move || show_only_with_lyrics.get()
                        on:change=move |ev| {
                            let checked = event_target_checked(&ev);
                            show_only_with_lyrics.set(checked);
                        }
                    />
                    " Len skladby s textom"
                </label>
            </div>
            <div class="live-catalog-error">
                {move || {
                    let e = error_msg.get();
                    if e.is_empty() {
                        view! { <span></span> }.into_any()
                    } else {
                        view! {
                            <crate::components::state_block::StateBlock
                                kind=crate::components::state_block::StateKind::Error(e)
                            />
                        }
                            .into_any()
                    }
                }}
            </div>
            <div class="song-list">
                <For
                    each=visible
                    key=|s| {
                        (
                            s["video_id"].as_i64().unwrap_or(0),
                            s["has_lyrics"].as_bool().unwrap_or(false),
                            s["is_stale"].as_bool().unwrap_or(false),
                            s["lyrics_reference"].as_bool().unwrap_or(false),
                        )
                    }
                    children=move |song| {
                        let video_id = song["video_id"].as_i64().unwrap_or(0);
                        let title = song["song"].as_str().unwrap_or("—").to_string();
                        let artist = song["artist"].as_str().unwrap_or_default().to_string();
                        let has_lyrics = song["has_lyrics"].as_bool().unwrap_or(false);
                        let is_stale = song["is_stale"].as_bool().unwrap_or(false);
                        let is_ref = song["lyrics_reference"].as_bool().unwrap_or(false);
                        let chips = vec![ChipView::new(text_chip(has_lyrics, is_ref, is_stale))];
                        view! {
                            <SongRow video_id=video_id title=title artist=artist chips=chips>
                                <button
                                    type="button"
                                    class="song-row-btn"
                                    title="Pridať do set listu"
                                    on:click=move |_| {
                                        leptos::task::spawn_local(async move {
                                            match api::post_live_add_item(target_playlist_id, video_id)
                                                .await
                                            {
                                                Ok(_) => on_added.run(video_id),
                                                Err(e) => error_msg.set(e),
                                            }
                                        });
                                    }
                                >
                                    "+ Pridať"
                                </button>
                            </SongRow>
                        }
                    }
                />
            </div>
        </div>
    }
}
