//! #194 ROUND 3c: the ONE playlist chooser + selection state, used on every
//! page that picks a playlist (Dashboard, Live, Lyrics). It replaces the
//! Dashboard-only `playlist_selector.rs`, the hardcoded `name == "ytlive"`
//! lookup in `pages/live.rs`, and drives the per-playlist sections on
//! `pages/lyrics.rs` — one chooser, one selection state
//! (`store.selected_playlist`), rendered identically wherever a playlist is
//! chosen.
//!
//! Testids (set INSIDE the component, never injected by a caller — #194 rule):
//! `playlist-picker` (container), `playlist-picker-item` (each desktop row),
//! `playlist-picker-select` (the ≤700 px mobile `<select>`), `playlist-picker-
//! list` (the desktop list). The CSS class names are kept (`playlist-selector-*`)
//! so the existing `.playlist-selector-row .lock-badge` style + locators resolve.
//!
//! #170: both the desktop list and the mobile `<select>` use a keyed `<For>`
//! whose `each` reads ONLY `store.playlists` (never `store.now_playing`), so a
//! 500 ms position tick does NOT recreate/re-order the rows — the glyph +
//! `selected` class are per-row reactive and update in place.

use leptos::prelude::*;
use sp_core::models::Playlist;

use crate::components::{ndi_health, selection};
use crate::store::DashboardStore;

/// The visible, ordered playlist set for the picker: filtered by `kinds` (when
/// given, e.g. `["custom"]` on Live), then alphabetical + stable (#170).
fn visible_playlists(all: Vec<Playlist>, kinds: &Option<Vec<String>>) -> Vec<Playlist> {
    let filtered: Vec<Playlist> = match kinds {
        Some(ks) => all.into_iter().filter(|p| ks.contains(&p.kind)).collect(),
        None => all,
    };
    selection::ordered(&filtered)
}

#[component]
pub fn PlaylistPicker(
    /// Restrict the chooser to these playlist `kind`s (e.g. `["custom"]` on the
    /// Live page so only live-kind playlists appear). Omit for every playlist
    /// (Dashboard, Lyrics).
    #[prop(optional)]
    kinds: Option<Vec<String>>,
) -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");
    // Each `<For each=…>` needs its OWN closure (a closure moved into the first
    // `each` cannot be reused in the second), so clone the filter for each.
    let kinds_mobile = kinds.clone();
    let kinds_desktop = kinds;

    view! {
        <div class="playlist-selector-panel" data-testid="playlist-picker">
            // Mobile (≤700 px): a native dropdown above the work area.
            <select
                class="playlist-select-mobile"
                data-testid="playlist-picker-select"
                on:change=move |ev| {
                    if let Ok(id) = event_target_value(&ev).parse::<i64>() {
                        selection::select(store, id);
                    }
                }
            >
                <For
                    each=move || visible_playlists(store.playlists.get(), &kinds_mobile)
                    key=|p| (p.id, p.name.clone(), p.ndi_output_name.clone())
                    children=move |p| {
                        let pid = p.id;
                        let name = p.name.clone();
                        let ndi = p.ndi_output_name.clone();
                        view! {
                            <option
                                value=pid.to_string()
                                selected=move || store.selected_playlist.get() == Some(pid)
                            >
                                {move || {
                                    let glyph = selection::playback_glyph(
                                        &store.now_playing.get(),
                                        pid,
                                    );
                                    format!("{glyph} {name} ({ndi})")
                                }}
                            </option>
                        }
                    }
                />
            </select>

            // Desktop: a clickable list panel.
            <div class="playlist-selector-list" data-testid="playlist-picker-list">
                <For
                    each=move || visible_playlists(store.playlists.get(), &kinds_desktop)
                    key=|p| (p.id, p.name.clone(), p.ndi_output_name.clone())
                    children=move |p| {
                        let pid = p.id;
                        let name = p.name.clone();
                        let ndi = p.ndi_output_name.clone();
                        let ndi_name = p.ndi_output_name.clone();
                        view! {
                            <button
                                class="playlist-selector-row"
                                data-testid="playlist-picker-item"
                                data-playlist-id=pid.to_string()
                                class:selected=move || {
                                    store.selected_playlist.get() == Some(pid)
                                }
                                on:click=move |_| selection::select(store, pid)
                            >
                                <span class="sel-glyph">
                                    {move || selection::playback_glyph(&store.now_playing.get(), pid)}
                                </span>
                                <span class="sel-name">{name}</span>
                                <span class="sel-ndi">{ndi}</span>
                                {
                                    let ndi_name = ndi_name.clone();
                                    move || {
                                        // #164: badge only where actionable
                                        // (live pacing-enabled output).
                                        store
                                            .ndi_health
                                            .get()
                                            .into_iter()
                                            .find(|o| o.ndi_name == ndi_name)
                                            .filter(ndi_health::should_show_lock_badge)
                                            .map(|o| view! { <ndi_health::LockBadge output=o /> })
                                    }
                                }
                            </button>
                        }
                    }
                />
            </div>
        </div>
    }
}
