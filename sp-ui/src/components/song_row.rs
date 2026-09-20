//! #194 ROUND 2: the ONE song-row renderer for every page.
//!
//! Title / artist, a primary play action (same glyph + Slovak tooltip
//! everywhere, disabled while the file is not ready), the shared status chips,
//! and a page-supplied actions slot. It replaces the five divergent row bodies
//! (`video_list`, `live_setlist`, `live_catalog`, `lyrics_song_row`,
//! `dabing_list`) so a song looks and behaves the same on the Dashboard, Live,
//! Lyrics and Dabing pages. A `<div>` flex row (not a `<table>` row) so the same
//! component drops into every page.

use leptos::prelude::*;

use crate::components::status_chips::{ChipView, StatusChips};

#[component]
pub fn SongRow(
    video_id: i64,
    /// The song title (already falling back to the youtube id when unknown).
    title: String,
    /// The artist; empty when unknown (then only the title shows).
    #[prop(optional)]
    artist: String,
    /// The status chips for this row (built from `sp_core::status_chip`).
    chips: Vec<ChipView>,
    /// Primary play action. `None` → no play button (e.g. the catalog, whose
    /// primary action is "add to set list"). `#[prop(into)]` so a caller can pass
    /// a bare `Callback` (wrapped to `Some`) or omit it.
    #[prop(optional, into)]
    on_play: Option<Callback<()>>,
    /// Whether the file is ready to play; the play button is disabled otherwise.
    #[prop(optional)]
    play_ready: bool,
    /// Page-supplied action controls (edit / reprocess / dub toggle / delete /
    /// add …). Each page passes only the actions it needs.
    #[prop(optional)]
    children: Option<Children>,
) -> impl IntoView {
    let title_line = if artist.trim().is_empty() {
        title
    } else {
        format!("{title} — {artist}")
    };

    view! {
        <div class="song-row" data-testid="song-row" data-video-id=video_id.to_string()>
            {on_play
                .map(|cb| {
                    let tip = if play_ready {
                        "Prehrať teraz"
                    } else {
                        "Ešte nespracované — nedá sa prehrať"
                    };
                    view! {
                        <button
                            type="button"
                            class="song-row-btn song-row-btn-play"
                            data-testid="song-row-play"
                            title=tip
                            prop:disabled=!play_ready
                            on:click=move |_| cb.run(())
                        >
                            "▶"
                        </button>
                    }
                })}
            <span class="song-row-title" data-testid="song-row-title">
                {title_line}
            </span>
            <StatusChips chips=chips />
            {children
                .map(|c| {
                    view! { <div class="song-row-actions">{c()}</div> }
                })}
        </div>
    }
}
