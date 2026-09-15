//! #165: the single work area. A thin wrapper that reuses `PlaylistCard`'s
//! internals (now-playing, #15 preview, #163 karaoke panel, playback + karaoke
//! controls, #134/#136 video list) for the ONE selected playlist, full width.
//! Above it sits the always-rendered, layout-stable "Práve hrá" strip.

use leptos::prelude::*;

use crate::components::playlist_card;
use crate::components::selection;
use crate::store::DashboardStore;

#[component]
pub fn PlaylistWorkspace() -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");

    view! {
        <div class="playlist-workspace" data-testid="playlist-workspace">
            // "Práve hrá" strip — ALWAYS in the DOM with a reserved height
            // (`.now-playing-strip` min-height) so nothing below it jumps; only
            // the text/button inside swaps (#163 layout-stable pattern).
            <div class="now-playing-strip" data-testid="now-playing-strip">
                {move || {
                    let pls = store.playlists.get();
                    let np = store.now_playing.get();
                    let selected = store.selected_playlist.get();
                    match selection::first_playing(&pls, &np) {
                        Some(pid) if Some(pid) != selected => {
                            // A DIFFERENT playlist than the selected one is
                            // playing — advertise it + a jump button.
                            let name = pls
                                .iter()
                                .find(|p| p.id == pid)
                                .map(|p| p.name.clone())
                                .unwrap_or_default();
                            let song = np
                                .get(&pid)
                                .map(|i| i.song.clone())
                                .filter(|s| !s.is_empty())
                                .unwrap_or_else(|| "…".to_string());
                            view! {
                                <span class="strip-live">
                                    {format!("▶ {name} — {song}")}
                                </span>
                                <button
                                    class="strip-goto"
                                    data-testid="strip-goto"
                                    on:click=move |_| selection::select(store, pid)
                                >
                                    "Prejsť naň"
                                </button>
                            }
                                .into_any()
                        }
                        Some(_) => {
                            // The selected playlist is the one playing.
                            view! { <span class="strip-quiet">"Vybraný playlist hrá"</span> }
                                .into_any()
                        }
                        None => {
                            view! { <span class="strip-quiet">"Nič nehrá"</span> }.into_any()
                        }
                    }
                }}
            </div>

            // The single work area for the selected playlist. Reuses PlaylistCard
            // (no duplication); badge off — the #164 badge lives in the selector
            // rows + header summary, not here.
            {move || {
                let pls = store.playlists.get();
                let selected = store.selected_playlist.get();
                match selected.and_then(|id| pls.into_iter().find(|p| p.id == id)) {
                    Some(pl) => {
                        view! { <playlist_card::PlaylistCard playlist=pl show_badge=false /> }
                            .into_any()
                    }
                    None => {
                        view! {
                            <div class="workspace-empty" data-testid="workspace-empty">
                                "Žiadne playlisty"
                            </div>
                        }
                            .into_any()
                    }
                }
            }}
        </div>
    }
}
