//! Section showing all songs for one playlist with their lyrics state.

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::components::lyrics_song_row::LyricsSongRow;
use crate::store::{DashboardStore, LyricsSongEntry, ReprocessOutcome};

#[component]
pub fn LyricsPlaylistSection(
    playlist_id: i64,
    playlist_name: String,
    on_details: Callback<i64>,
) -> impl IntoView {
    let songs: RwSignal<Vec<LyricsSongEntry>> = RwSignal::new(Vec::new());

    spawn_local({
        let songs = songs;
        async move {
            if let Ok(items) = api::get_lyrics_songs(Some(playlist_id)).await {
                let parsed: Vec<LyricsSongEntry> = items
                    .into_iter()
                    .filter_map(|v| serde_json::from_value(v).ok())
                    .collect();
                songs.set(parsed);
            }
        }
    });

    let pid = playlist_id;
    let store = expect_context::<DashboardStore>();
    let last_reprocess = store.last_reprocess;
    let on_reprocess_playlist = move |_| {
        spawn_local(async move {
            if let Ok(v) = api::post_reprocess_playlist(pid).await {
                let queued = v.get("queued").and_then(|x| x.as_i64()).unwrap_or(0);
                let blocked = v
                    .get("blocked_by_asr_gap")
                    .and_then(|x| x.as_i64())
                    .unwrap_or(0);
                last_reprocess.set(Some(ReprocessOutcome {
                    queued,
                    blocked_by_asr_gap: blocked,
                }));
            }
        });
    };

    view! {
        <section class="lyrics-playlist-section">
            <h3>
                {playlist_name.clone()}
                <button on:click=on_reprocess_playlist>"Reprocess playlist"</button>
            </h3>
            <div class="lyrics-songs">
                <For
                    each=move || songs.get()
                    key=|e: &LyricsSongEntry| e.video_id
                    let:entry
                >
                    <LyricsSongRow entry=entry on_details=on_details />
                </For>
            </div>
        </section>
    }
}
