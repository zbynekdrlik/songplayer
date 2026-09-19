//! Main dashboard page. #165: a playlist SELECTOR + ONE work area (the playing
//! playlist preselected) instead of a grid of every playlist card.

use leptos::prelude::*;
use sp_core::models::Playlist;

use crate::api;
use crate::components::{
    download_queue, lan_address, ndi_health, obs_status, playlist_selector, playlist_workspace,
    resolume_health, selection,
};
use crate::store::DashboardStore;

#[component]
pub fn DashboardPage() -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");

    // Fetch playlists on mount.
    let _load = Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            if let Ok(playlists) = api::get::<Vec<Playlist>>("/api/v1/playlists").await {
                store.playlists.set(playlists);
            }
        });
    });

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
                <h1>"Playlists"</h1>
                <lan_address::LanAddress />
                <obs_status::ObsStatus />
                <ndi_health::GlobalLockBadge />
                <resolume_health::ResolumeHealthCard />
            </div>

            <div class="error-banner">
                {move || {
                    let errs = store.errors.get();
                    if errs.is_empty() {
                        view! { <span></span> }.into_any()
                    } else {
                        let last = errs.last().cloned().unwrap_or_default();
                        view! { <div class="error-msg">{last}</div> }.into_any()
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
