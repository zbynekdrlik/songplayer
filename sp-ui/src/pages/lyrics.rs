//! Lyrics management page showing pipeline queue and per-playlist song rows.

use leptos::prelude::*;

use crate::components::lyrics_playlist_section::LyricsPlaylistSection;
use crate::components::lyrics_queue_card::LyricsQueueCard;
use crate::components::lyrics_song_detail::LyricsSongDetailModal;
use crate::store::DashboardStore;

#[component]
pub fn LyricsPage() -> impl IntoView {
    let store = expect_context::<DashboardStore>();
    let selected = RwSignal::new(None::<i64>);

    let on_details: Callback<i64> = Callback::new(move |id| selected.set(Some(id)));
    let on_close: Callback<()> = Callback::new(move |_| selected.set(None));

    view! {
        <div class="lyrics-page">
            <LyricsQueueCard />
            <For
                each=move || store.playlists.get()
                key=|p| p.id
                children=move |playlist| {
                    view! {
                        <LyricsPlaylistSection
                            playlist_id=playlist.id
                            playlist_name=playlist.name.clone()
                            on_details=on_details
                        />
                    }
                }
            />
            {move || {
                selected
                    .get()
                    .map(|id| {
                        view! { <LyricsSongDetailModal video_id=id on_close=on_close /> }
                    })
            }}
        </div>
    }
}
