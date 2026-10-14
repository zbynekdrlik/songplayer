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
//! 500 ms position tick does NOT recreate/re-order the rows — the state word +
//! `selected` class are per-row reactive and update in place.
//!
//! #266: a row says its playlist's state in WORDS (`playlist-picker-status`,
//! `data-status`: "NA PROGRAME" in the PROGRAM row's red, "hrá mimo
//! programu", "pozastavené", "nehrá"; nothing until the state is known), and
//! the mobile `<option>` carries the same word — never a ▶ / ⏸ glyph, which
//! reads as the button that does that action (the owner's ROZHODNUTÉ
//! 13.10.2026). Each row's word is a `Memo` (`selection::playlist_status`), so
//! a position tick touches no row.

use leptos::prelude::*;
use sp_core::models::Playlist;
use sp_core::player_view;

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
                // `selected` on an <option> is only honoured at parse time; the
                // live selection (desktop row click, auto-follow, ?playlist=)
                // must drive the select's VALUE property (0.60.0 review).
                prop:value=move || {
                    store.selected_playlist.get().map(|i| i.to_string()).unwrap_or_default()
                }
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
                        let status = Memo::new(move |_| selection::playlist_status(store, pid));
                        view! {
                            <option
                                value=pid.to_string()
                                selected=move || store.selected_playlist.get() == Some(pid)
                                data-status=move || status.get().code()
                            >
                                {move || player_view::picker_option_text(&name, &ndi, status.get())}
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
                        let status = Memo::new(move |_| selection::playlist_status(store, pid));
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
                                // #266: the name, and under it the state in words.
                                <span class="sel-main">
                                    <span class="sel-name">{name}</span>
                                    <span
                                        class=move || format!("sel-status status-{}", status.get().code())
                                        data-testid="playlist-picker-status"
                                        data-status=move || status.get().code()
                                    >
                                        {move || status.get().label()}
                                    </span>
                                </span>
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
