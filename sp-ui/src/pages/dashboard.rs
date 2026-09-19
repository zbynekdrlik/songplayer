//! Main dashboard page. #165: a playlist SELECTOR + ONE work area (the playing
//! playlist preselected) instead of a grid of every playlist card.

use leptos::prelude::*;

use crate::components::{download_queue, playlist_selector, playlist_workspace, selection};
use crate::store::DashboardStore;

#[component]
pub fn DashboardPage() -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");

    // #194 r3: playlists are loaded once at the app level (`App`), so every
    // page — not only the Dashboard — has `store.playlists`. The Dashboard no
    // longer fetches them itself.

    // #165: auto-follow the playing playlist for the INITIAL selection. Runs
    // until the selection is pinned (a user click / `<select>` / "Prejsť", or a
    // value restored from the URL/localStorage in `App`) AND still valid.
    // `App` seeds a persisted selection with `selection_pinned = true`, so a
    // reload keeps the operator's choice; a fresh load (no persisted value) has
    // `pinned = false`, so it defaults to the currently-playing playlist, then
    // the first by name. Reads of `selected_playlist` are UNTRACKED so the
    // Effect never re-triggers on its own write.
    let _auto = Effect::new(move |_| {
        let pls = store.playlists.get();
        let np = store.now_playing.get();
        let pinned = store.selection_pinned.get();
        if pls.is_empty() {
            return;
        }
        let sel = store.selected_playlist.get_untracked();
        let valid = sel.is_some_and(|id| pls.iter().any(|p| p.id == id));
        if pinned && valid {
            return;
        }
        if let Some(target) = selection::choose_default(&pls, &np)
            && sel != Some(target)
        {
            store.selected_playlist.set(Some(target));
        }
    });

    view! {
        <div class="dashboard">
            <div class="dashboard-header">
                <h1>"Playlisty"</h1>
            </div>

            <div class="error-banner">
                {move || {
                    let errs = store.errors.get();
                    if errs.is_empty() {
                        view! { <span></span> }.into_any()
                    } else {
                        let last = errs.last().cloned().unwrap_or_default();
                        view! {
                            <crate::components::state_block::StateBlock kind=crate::components::state_block::StateKind::Error(last) />
                        }
                        .into_any()
                    }
                }}
            </div>

            <div class="dashboard-body">
                <playlist_selector::PlaylistSelector />
                <playlist_workspace::PlaylistWorkspace />
            </div>

            <download_queue::DownloadQueue />
        </div>
    }
}
