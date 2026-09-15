//! #165: the playlist selector — the left panel (a `<select>` above the work
//! area on ≤700 px) that lists every playlist and drives which one the single
//! work area shows. Rows are ordered playing-first then by name; each row shows
//! a playback glyph (▶ / ⏸ / –), the name, the NDI output/scene, and the #164
//! genlock badge (only where actionable). Both the desktop list and the mobile
//! `<select>` are always in the DOM; CSS toggles which is visible.

use leptos::prelude::*;

use crate::components::{ndi_health, selection};
use crate::store::DashboardStore;

#[component]
pub fn PlaylistSelector() -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");

    view! {
        <div class="playlist-selector-panel">
            // Mobile (≤700 px): a native dropdown above the work area.
            <select
                class="playlist-select-mobile"
                data-testid="playlist-select"
                on:change=move |ev| {
                    if let Ok(id) = event_target_value(&ev).parse::<i64>() {
                        selection::select(store, id);
                    }
                }
            >
                {move || {
                    let pls = store.playlists.get();
                    let np = store.now_playing.get();
                    selection::ordered(&pls, &np)
                        .into_iter()
                        .map(|p| {
                            let pid = p.id;
                            let glyph = selection::playback_glyph(&np, pid);
                            let label = format!("{glyph} {} ({})", p.name, p.ndi_output_name);
                            view! {
                                <option
                                    value=pid.to_string()
                                    selected=move || store.selected_playlist.get() == Some(pid)
                                >
                                    {label}
                                </option>
                            }
                        })
                        .collect_view()
                }}
            </select>

            // Desktop: a clickable list panel.
            <div class="playlist-selector-list" data-testid="playlist-selector-list">
                {move || {
                    let pls = store.playlists.get();
                    let np = store.now_playing.get();
                    selection::ordered(&pls, &np)
                        .into_iter()
                        .map(|p| {
                            let pid = p.id;
                            let name = p.name.clone();
                            let ndi = p.ndi_output_name.clone();
                            let ndi_name = p.ndi_output_name.clone();
                            let glyph = selection::playback_glyph(&np, pid);
                            view! {
                                <button
                                    class="playlist-selector-row"
                                    data-testid="playlist-selector-row"
                                    data-playlist-id=pid.to_string()
                                    class:selected=move || {
                                        store.selected_playlist.get() == Some(pid)
                                    }
                                    on:click=move |_| selection::select(store, pid)
                                >
                                    <span class="sel-glyph">{glyph}</span>
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
                        })
                        .collect_view()
                }}
            </div>
        </div>
    }
}
