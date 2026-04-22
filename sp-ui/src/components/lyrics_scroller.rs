//! Scrollable lyrics list for /live. Each line is a button — tap/click
//! jumps playback to that line's start_ms via POST /seek.

use leptos::prelude::*;
use sp_core::lyrics::LyricsTrack;

use crate::store::DashboardStore;

#[component]
pub fn LyricsScroller(playlist_id: i64, store: DashboardStore) -> impl IntoView {
    // Current video id + position — react to BOTH so we re-fetch on song
    // change and re-highlight on position tick.
    let video_id = move || {
        store
            .now_playing
            .get()
            .get(&playlist_id)
            .map(|n| n.video_id)
    };
    let position = move || {
        store
            .now_playing
            .get()
            .get(&playlist_id)
            .map(|n| n.position_ms)
            .unwrap_or(0)
    };

    // Fetched track kept in a signal so we can re-fetch on video_id change.
    let track = RwSignal::new(None::<LyricsTrack>);
    let fetch_error = RwSignal::new(String::new());

    let _fetch = Effect::new(move |_| {
        let Some(vid) = video_id() else {
            track.set(None);
            return;
        };
        leptos::task::spawn_local(async move {
            match crate::api::get_video_lyrics(vid).await {
                Ok(t) => {
                    track.set(Some(t));
                    fetch_error.set(String::new());
                }
                Err(e) => {
                    track.set(None);
                    fetch_error.set(e);
                }
            }
        });
    });

    let do_seek = move |ms: u64| {
        leptos::task::spawn_local(async move {
            let _ = crate::api::seek_playlist(playlist_id, ms).await;
        });
    };

    let current_line_idx = move || -> Option<usize> {
        let t = track.get()?;
        let pos = position();
        // Walk back — find the last line whose start_ms <= pos. Simple O(N)
        // which is fine for <200 lines per song.
        t.lines.iter().rposition(|l| l.start_ms <= pos)
    };

    view! {
        <div class="lyrics-scroller">
            {move || {
                if !fetch_error.get().is_empty() {
                    view! { <div class="lyrics-error">{fetch_error.get()}</div> }.into_any()
                } else if let Some(t) = track.get() {
                    let lines = t.lines.clone();
                    view! {
                        <ol class="lyrics-list">
                            <For
                                each=move || lines.clone().into_iter().enumerate().collect::<Vec<_>>()
                                key=|(i, l)| (*i, l.start_ms)
                                children=move |(i, line)| {
                                    let start = line.start_ms;
                                    let en = line.en.clone();
                                    let sk = line.sk.clone();
                                    let is_current = move || current_line_idx() == Some(i);
                                    view! {
                                        <li>
                                            <button
                                                class=move || if is_current() { "lyr-line lyr-current" } else { "lyr-line" }
                                                on:click=move |_| do_seek(start)
                                            >
                                                <span class="lyr-en">{en.clone()}</span>
                                                {sk.clone().map(|s| view! { <span class="lyr-sk">{s}</span> })}
                                            </button>
                                        </li>
                                    }
                                }
                            />
                        </ol>
                    }.into_any()
                } else {
                    view! { <div class="lyrics-empty">"No lyrics loaded"</div> }.into_any()
                }
            }}
        </div>
    }
}
