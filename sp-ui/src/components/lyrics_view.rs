//! #194 ROUND 3c: the ONE lyrics surface for the whole app.
//!
//! It replaces BOTH `karaoke_panel.rs` (the Dashboard's 4-line preview) and
//! `lyrics_scroller.rs` (the Live tappable list) with a single scrollable,
//! tappable line list: the song's `LyricsTrack` rendered as `<li>` buttons, the
//! CURRENT line highlighted by a `Memo` of the live position, tapping a line
//! seeks. It is rendered in the `Player`'s lyrics slot (Dashboard, Live, Dabing —
//! the dub's subtitles are just its `LyricsTrack`) and in the Lyrics details
//! view. One surface everywhere = the "jednotná aplikácia" rule; the old
//! Dashboard-only word-highlight vs Live-only tappable-list split is gone.
//!
//! Inputs: `playlist_id` (the player passes it; the current video + seek target
//! are resolved from `store.now_playing[playlist_id]`) OR `video_id` (the details
//! view passes it explicitly; the seek target is whichever playlist is playing
//! that video, if any). A live position tick updates only the highlighted `<li>`
//! (the active-line index is a `Memo`) — it never re-creates the `lyrics-view`
//! root or rebuilds the `<ol>` (sp-ui-frontend.md "a live data tick must update
//! text only").

use leptos::prelude::*;
use sp_core::lyrics::LyricsTrack;

use crate::components::state_block::{StateBlock, StateKind};
use crate::store::DashboardStore;

#[component]
pub fn LyricsView(
    /// The playlist whose now-playing video's lyrics to show + seek (the Player).
    #[prop(optional)]
    playlist_id: Option<i64>,
    /// An explicit video whose lyrics to show (the Lyrics details view).
    #[prop(optional)]
    video_id: Option<i64>,
) -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");

    // The video to render: an explicit `video_id`, else the now-playing video of
    // `playlist_id`. A `Memo` so a position tick (which does not change the video)
    // never re-runs the fetch Effect below. `video_id == 0` is the state-only
    // placeholder entry → treated as "no video".
    let effective_vid = Memo::new(move |_| {
        video_id.or_else(|| {
            playlist_id
                .and_then(|pid| store.now_playing.get().get(&pid).map(|n| n.video_id))
                .filter(|v| *v != 0)
        })
    });

    // The playlist to read the live position from and to seek: the explicit
    // `playlist_id` (the Player), else whichever playlist is playing the effective
    // video (the details view, when that song happens to be on air).
    let seek_pid = move || {
        playlist_id.or_else(|| {
            effective_vid().and_then(|v| {
                store
                    .now_playing
                    .get()
                    .iter()
                    .find(|(_, n)| n.video_id == v)
                    .map(|(pid, _)| *pid)
            })
        })
    };
    let position = move || {
        seek_pid()
            .and_then(|pid| store.now_playing.get().get(&pid).map(|n| n.position_ms))
            .unwrap_or(0)
    };

    let track = RwSignal::new(None::<LyricsTrack>);
    // Re-fetch only when the effective video changes (the Effect reads the Memo).
    // `try_set` so a fetch that lands after the view is disposed stops cleanly.
    Effect::new(move |_| match effective_vid.get() {
        None => {
            let _ = track.try_set(None);
        }
        Some(v) => {
            leptos::task::spawn_local(async move {
                match crate::api::get_video_lyrics(v).await {
                    Ok(t) => {
                        let _ = track.try_set(Some(t));
                    }
                    Err(_) => {
                        let _ = track.try_set(None);
                    }
                }
            });
        }
    });

    // Active line = a Memo of the live position, so a tick flips only the
    // highlighted <li> class, never rebuilds the <ol>.
    let current_idx =
        Memo::new(move |_| track.get().as_ref().and_then(|t| t.current_line_index(position())));

    let do_seek = move |ms: u64| {
        if let Some(pid) = seek_pid() {
            leptos::task::spawn_local(async move {
                let _ = crate::api::seek_playlist(pid, ms).await;
            });
        }
    };

    view! {
        <div class="lyrics-view lyrics-view-scroll" data-testid="lyrics-view">
            {move || match track.get() {
                None => view! {
                    <StateBlock kind=StateKind::Empty empty_label="Žiadny text".to_string() />
                }
                .into_any(),
                Some(t) => {
                    let items = t
                        .lines
                        .iter()
                        .enumerate()
                        .map(|(idx, ln)| {
                            let start = ln.start_ms;
                            let en = ln.en.clone();
                            let sk = ln.sk.clone();
                            view! {
                                <li>
                                    <button
                                        class="lyr-line"
                                        class:lyr-current=move || current_idx.get() == Some(idx)
                                        on:click=move |_| do_seek(start)
                                    >
                                        <span class="lyr-en">{en}</span>
                                        {sk.map(|s| view! { <span class="lyr-sk">{s}</span> })}
                                    </button>
                                </li>
                            }
                        })
                        .collect_view();
                    view! { <ol class="lyrics-list">{items}</ol> }.into_any()
                }
            }}
        </div>
    }
}
