//! Scrollable lyrics list for /live. Each line is a button — tap/click
//! jumps playback to that line's start_ms via POST /seek.

use leptos::prelude::*;
use sp_core::lyrics::LyricsTrack;

use crate::store::DashboardStore;

#[component]
pub fn LyricsScroller(playlist_id: i64, store: DashboardStore) -> impl IntoView {
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

    // Reactive renderer: rebuild the list of <li> views every time track or
    // position changes. O(N) but N < 200 per song — fine.
    view! {
        <div class="lyrics-scroller">
            {move || {
                let err = fetch_error.get();
                if !err.is_empty() {
                    return view! { <div class="lyrics-error">{err}</div> }.into_any();
                }
                let Some(t) = track.get() else {
                    return view! { <div class="lyrics-empty">"No lyrics loaded"</div> }
                        .into_any();
                };
                let pos = position();
                let current_idx: Option<usize> =
                    t.lines.iter().rposition(|l| l.start_ms <= pos);
                let mut items = Vec::with_capacity(t.lines.len());
                for (idx, ln) in t.lines.iter().enumerate() {
                    let start = ln.start_ms;
                    let en_text = ln.en.clone();
                    let sk_text = ln.sk.clone();
                    let class_str = if current_idx == Some(idx) {
                        "lyr-line lyr-current"
                    } else {
                        "lyr-line"
                    };
                    let do_seek = do_seek.clone();
                    items.push(
                        view! {
                            <li>
                                <button
                                    class=class_str
                                    on:click=move |_| do_seek(start)
                                >
                                    <span class="lyr-en">{en_text}</span>
                                    {sk_text.map(|s| view! { <span class="lyr-sk">{s}</span> })}
                                </button>
                            </li>
                        }
                        .into_any(),
                    );
                }
                view! { <ol class="lyrics-list">{items}</ol> }.into_any()
            }}
        </div>
    }
}
