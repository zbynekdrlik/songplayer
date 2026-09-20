//! /live page: mobile-first layout for operating the live setlist from a phone
//! during worship. The shared `PlaylistPicker` (live-kind only) is up top, the
//! set list is the primary control surface, the compact `Player` sits under it,
//! and adding songs is tucked into a collapsible section at the bottom.
//!
//! #194 ROUND 3c: the live playlist is chosen through the SHARED `PlaylistPicker`
//! (filtered to the live-kind `"custom"` playlists) — no hardcoded
//! `name == "ytlive"` lookup. The effective id is the shared selection when it
//! points at a custom playlist, else the first custom playlist.

use leptos::prelude::*;

use crate::components::import_box::{ImportBox, ImportTarget};
use crate::components::live_catalog::LiveCatalog;
use crate::components::live_setlist::LiveSetList;
use crate::components::player::Player;
use crate::components::playlist_picker::PlaylistPicker;
use crate::components::selection;
use crate::components::state_block::{StateBlock, StateKind};
use crate::store::DashboardStore;

#[component]
pub fn LivePage() -> impl IntoView {
    let store = expect_context::<DashboardStore>();
    let set_list_version = RwSignal::new(0u64);
    // Collapse the "Add songs" panel by default — during a live set the operator
    // only needs the catalog/URL input occasionally.
    let add_open = RwSignal::new(false);

    // The live playlist: the shared selection when it is a live-kind ("custom")
    // playlist, else the first custom playlist. A `Memo` so a now-playing tick
    // never re-mounts the page body — only a real change of the resolved id does.
    let live_pid = Memo::new(move |_| {
        selection::selection_or_first_of_kind(
            &store.playlists.get(),
            store.selected_playlist.get(),
            "custom",
        )
    });

    // The live playlist is seeded `single` server-side (startup.rs) — the page
    // no longer forces the mode on every mount, so the operator's pick in the
    // shared Player's mode select survives navigation (0.60.0 review).

    let bump: Callback<()> = Callback::new(move |_| {
        set_list_version.update(|v| *v += 1);
    });
    let bump_after_add: Callback<i64> = Callback::new(move |_vid| {
        set_list_version.update(|v| *v += 1);
    });

    view! {
        <div class="live-page">
            // #194 r3c: the ONE playlist chooser, filtered to live-kind playlists.
            <PlaylistPicker kinds=vec!["custom".to_string()] />
            {move || match live_pid.get() {
                None => view! { <StateBlock kind=StateKind::Loading /> }.into_any(),
                Some(id) => view! {
                    <>
                        // 1. Primary control surface: tap a song to play it,
                        //    tap ✕ to remove. Big touch targets for finger ops.
                        <section class="live-section live-section-setlist">
                            <LiveSetList
                                playlist_id=id
                                refresh=Signal::from(set_list_version)
                                on_changed=bump
                            />
                        </section>

                        // 2. The ONE shared player (now-playing, badge, seek,
                        //    transport, mode, preview, mixer) + the lyric line.
                        <section class="live-section live-section-player">
                            // The Player now carries the shared LyricsView in its
                            // own slot — no separate lyric surface on the page.
                            <Player playlist_id=id />
                        </section>

                        // 3. "Add songs" is tucked into a collapsible panel.
                        <section class="live-section live-section-add">
                            <button
                                class="live-add-toggle"
                                on:click=move |_| add_open.update(|o| *o = !*o)
                            >
                                {move || if add_open.get() {
                                    "▾ Pridať skladby do zoznamu"
                                } else {
                                    "▸ Pridať skladby do zoznamu"
                                }}
                            </button>
                            <div
                                class="live-add-body"
                                class:open=move || add_open.get()
                            >
                                <ImportBox
                                    target=ImportTarget::Playlist(id)
                                    on_imported=Callback::new(move |_: (i64, String)| {
                                        set_list_version.update(|v| *v += 1);
                                    })
                                />
                                <LiveCatalog
                                    target_playlist_id=id
                                    _set_list_version=Signal::from(set_list_version)
                                    on_added=bump_after_add
                                />
                            </div>
                        </section>
                    </>
                }.into_any(),
            }}
        </div>
    }
}
