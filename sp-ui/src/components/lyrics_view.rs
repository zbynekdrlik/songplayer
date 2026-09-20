//! #194 ROUND 3c: the ONE lyrics surface, with a `mode` prop.
//!
//! - `LyricsMode::Compact` — the 4-line karaoke preview (previous / current with
//!   word-highlight / SK / next) read from the live WS now-playing lines for a
//!   playlist. Rendered in the `Player`'s lyrics slot on Dashboard, Live and
//!   Dabing (dub subtitles arrive the same way). Replaces `karaoke_panel.rs`.
//! - `LyricsMode::Scroll` — the full tappable line list from the song's
//!   `LyricsTrack`, the CURRENT line highlighted by a `Memo` of the live
//!   position, tap a line to seek. Rendered by the Lyrics details view. Replaces
//!   `lyrics_scroller.rs`.
//!
//! Both share one root `data-testid="lyrics-view"`. A live position tick updates
//! TEXT / the active-index Memo only — it never re-creates the `lyrics-view`
//! element or the scroll `<ol>` (sp-ui-frontend.md "a live data tick must update
//! text only").

use leptos::prelude::*;
use sp_core::lyrics::LyricsTrack;

use crate::components::state_block::{StateBlock, StateKind};
use crate::store::DashboardStore;

/// A non-breaking space so an empty compact line keeps its height (#163).
const NBSP: &str = "\u{00A0}";

/// Which lyrics surface to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LyricsMode {
    /// 4-line karaoke word-highlight preview (the Player's lyrics slot).
    Compact,
    /// Scrollable tappable line list with seek (the Lyrics details view).
    Scroll,
}

#[component]
pub fn LyricsView(
    mode: LyricsMode,
    /// The playlist whose live now-playing lines drive the Compact preview.
    #[prop(optional)]
    playlist_id: Option<i64>,
    /// The song whose full `LyricsTrack` the Scroll list renders.
    #[prop(optional)]
    video_id: Option<i64>,
) -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");

    match mode {
        LyricsMode::Compact => {
            let pid = playlist_id.unwrap_or(-1);
            let np = move || store.now_playing.get().get(&pid).cloned();
            let prev_line = move || {
                np()
                    .and_then(|i| i.prev_line_en)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| NBSP.to_string())
            };
            let next_line = move || {
                np()
                    .and_then(|i| i.next_line_en)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| NBSP.to_string())
            };
            let sk_line = move || {
                np()
                    .and_then(|i| i.line_sk)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| NBSP.to_string())
            };
            // The active WORD index is a Memo so a position tick that does not
            // change it never re-runs the current-line closure.
            let active_idx = Memo::new(move |_| np().and_then(|i| i.active_word_index).unwrap_or(0));
            let words =
                move || match np().and_then(|i| i.line_en).filter(|s| !s.is_empty()) {
                    Some(en) => en.split_whitespace().map(String::from).collect::<Vec<_>>(),
                    None => Vec::new(),
                };
            let current_view = move || {
                let ws = words();
                let idx = active_idx.get();
                if ws.is_empty() {
                    view! { <span class="karaoke-word">{NBSP}</span> }.into_any()
                } else {
                    ws.into_iter()
                        .enumerate()
                        .map(|(i, w)| {
                            let class = if i < idx {
                                "karaoke-word karaoke-word-past"
                            } else if i == idx {
                                "karaoke-word karaoke-word-active"
                            } else {
                                "karaoke-word karaoke-word-future"
                            };
                            view! { <span class=class>{w}{" "}</span> }
                        })
                        .collect_view()
                        .into_any()
                }
            };
            view! {
                <div class="lyrics-view lyrics-view-compact karaoke-panel" data-testid="lyrics-view">
                    <div class="karaoke-line karaoke-dim">{prev_line}</div>
                    <div class="karaoke-line karaoke-current">{current_view}</div>
                    <div class="karaoke-line karaoke-sk">{sk_line}</div>
                    <div class="karaoke-line karaoke-dim">{next_line}</div>
                </div>
            }
            .into_any()
        }
        LyricsMode::Scroll => {
            let vid = video_id;
            let track = RwSignal::new(None::<LyricsTrack>);
            // Fetch the track for this video. `try_set` so a fetch that lands
            // after the modal closed (signal disposed) stops instead of panics.
            Effect::new(move |_| {
                let Some(v) = vid else {
                    let _ = track.try_set(None);
                    return;
                };
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
            });
            // The playlist (if any) currently playing this video — for the live
            // position highlight + the tap-to-seek target.
            let playing_pid = move || {
                vid.and_then(|v| {
                    store
                        .now_playing
                        .get()
                        .iter()
                        .find(|(_, n)| n.video_id == v)
                        .map(|(pid, _)| *pid)
                })
            };
            let position = move || {
                playing_pid()
                    .and_then(|pid| store.now_playing.get().get(&pid).map(|n| n.position_ms))
                    .unwrap_or(0)
            };
            // Active line index = a Memo of the live position, so a tick updates
            // only the highlighted <li> class, never rebuilds the <ol>.
            let current_idx =
                Memo::new(move |_| track.get().as_ref().and_then(|t| t.current_line_index(position())));
            let do_seek = move |ms: u64| {
                if let Some(pid) = playing_pid() {
                    leptos::task::spawn_local(async move {
                        let _ = crate::api::seek_playlist(pid, ms).await;
                    });
                }
            };
            view! {
                <div class="lyrics-view lyrics-view-scroll" data-testid="lyrics-view">
                    {move || match track.get() {
                        None => view! {
                            <StateBlock
                                kind=StateKind::Empty
                                empty_label="Žiadny text".to_string()
                            />
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
            .into_any()
        }
    }
}
