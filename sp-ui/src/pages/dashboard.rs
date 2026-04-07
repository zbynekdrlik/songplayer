//! Main dashboard page showing playlists, now-playing, and download queue.

use leptos::prelude::*;
use sp_core::models::Playlist;

use crate::api;
use crate::components::{download_queue, obs_status, playlist_card};
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

    view! {
        <div class="dashboard">
            <div class="dashboard-header">
                <h1>"Playlists"</h1>
                <obs_status::ObsStatus />
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

            <div class="playlist-grid">
                <For
                    each=move || store.playlists.get()
                    key=|p| p.id
                    children=|playlist| {
                        view! { <playlist_card::PlaylistCard playlist=playlist /> }
                    }
                />
            </div>

            <download_queue::DownloadQueue />
        </div>
    }
}
