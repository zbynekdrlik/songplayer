//! #165: the single work area — one `PlaylistCard` for the selected playlist.
//! #194: the old "Práve hrá" now-playing strip is gone; now-playing lives in the
//! card's shared `Player` (one playback surface), and the dashboard's auto-follow
//! Effect already preselects the playing playlist on a fresh load.

use leptos::prelude::*;

use crate::components::playlist_card;
use crate::store::DashboardStore;

#[component]
pub fn PlaylistWorkspace() -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");

    view! {
        <div class="playlist-workspace" data-testid="playlist-workspace">
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
                            <crate::components::state_block::StateBlock
                                kind=crate::components::state_block::StateKind::Empty
                                empty_label="Žiadne playlisty".to_string()
                            />
                        }
                            .into_any()
                    }
                }
            }}
        </div>
    }
}
