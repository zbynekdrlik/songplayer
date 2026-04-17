//! Left pane of /live: lists all songs from the catalog with an optional
//! "has lyrics only" filter and a "+ Add" button per row that appends the
//! song to the given custom playlist's set list.

use leptos::prelude::*;

use crate::api;

#[component]
pub fn LiveCatalog(
    /// The custom playlist that add-clicks target.
    target_playlist_id: i64,
    /// Bumped by the parent whenever the set list changes; the catalog
    /// currently ignores it but the signal is carried so future changes
    /// (e.g. per-row "already-added" badges) can observe the edit.
    #[prop(into)] _set_list_version: Signal<u64>,
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
                Err(e) => error_msg.set(format!("failed to load songs: {e}")),
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
                <h2>"Catalog"</h2>
                <label>
                    <input
                        type="checkbox"
                        prop:checked=move || show_only_with_lyrics.get()
                        on:change=move |ev| {
                            let checked = event_target_checked(&ev);
                            show_only_with_lyrics.set(checked);
                        }
                    />
                    " Only songs with lyrics"
                </label>
            </div>
            <div class="live-catalog-error">{move || error_msg.get()}</div>
            <table class="live-catalog-table">
                <thead>
                    <tr>
                        <th>"Song"</th>
                        <th>"Artist"</th>
                        <th>"Lyrics"</th>
                        <th></th>
                    </tr>
                </thead>
                <tbody>
                    <For
                        each=visible
                        key=|s| s["video_id"].as_i64().unwrap_or(0)
                        children=move |song| {
                            let video_id = song["video_id"].as_i64().unwrap_or(0);
                            let title = song["song"].as_str().unwrap_or("—").to_string();
                            let artist = song["artist"].as_str().unwrap_or("—").to_string();
                            let has_lyrics = song["has_lyrics"].as_bool().unwrap_or(false);
                            let badge = if has_lyrics { "✓" } else { "" };
                            view! {
                                <tr>
                                    <td>{title}</td>
                                    <td>{artist}</td>
                                    <td>{badge}</td>
                                    <td>
                                        <button on:click=move |_| {
                                            leptos::task::spawn_local(async move {
                                                match api::post_live_add_item(
                                                    target_playlist_id, video_id,
                                                ).await {
                                                    Ok(_) => on_added.run(video_id),
                                                    Err(e) => error_msg.set(e),
                                                }
                                            });
                                        }>"+ Add"</button>
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
