//! /live page: mobile-first layout for operating the custom ytlive setlist
//! from a phone during worship. Set list is up top (primary control surface),
//! the compact player sits under it, and adding songs is tucked into a
//! collapsible section at the bottom so it stays out of the way during
//! performance.

use leptos::prelude::*;

use crate::api;
use crate::components::import_url_box::ImportUrlBox;
use crate::components::live_catalog::LiveCatalog;
use crate::components::live_setlist::LiveSetList;
use crate::components::lyrics_scroller::LyricsScroller;
use crate::components::now_playing_card::NowPlayingCard;
use crate::store::DashboardStore;

#[component]
pub fn LivePage() -> impl IntoView {
    let store = expect_context::<DashboardStore>();
    let ytlive_id = RwSignal::new(None::<i64>);
    let set_list_version = RwSignal::new(0u64);
    let error_msg = RwSignal::new(String::new());
    // Collapse the "Add songs" panel by default — during a live set the
    // operator only needs the catalog/URL input occasionally, so keeping
    // it folded leaves more room above the fold for the setlist + player.
    let add_open = RwSignal::new(false);

    // Resolve the ytlive playlist id on mount.
    let _resolve = Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            match api::get::<Vec<serde_json::Value>>("/api/v1/playlists").await {
                Ok(all) => {
                    let yt = all.iter().find(|p| p["name"] == "ytlive").cloned();
                    if let Some(p) = yt {
                        if let Some(id) = p["id"].as_i64() {
                            ytlive_id.set(Some(id));
                        }
                    } else {
                        error_msg.set(
                            "ytlive playlist missing — migration V13 not applied?".to_string(),
                        );
                    }
                }
                Err(e) => error_msg.set(format!("failed to load playlists: {e}")),
            }
        });
    });

    let bump: Callback<()> = Callback::new(move |_| {
        set_list_version.update(|v| *v += 1);
    });
    let bump_after_add: Callback<i64> = Callback::new(move |_vid| {
        set_list_version.update(|v| *v += 1);
    });

    view! {
        <div class="live-page">
            <div class="live-page-error">{move || error_msg.get()}</div>
            {move || match ytlive_id.get() {
                None => view! { <div>"Loading ytlive playlist…"</div> }.into_any(),
                Some(id) => view! {
                    <>
                        // 1. Primary control surface: tap a song to play it,
                        //    tap ✕ to remove. Big touch targets for finger ops.
                        <section class="live-section live-section-setlist">
                            <LiveSetList
                                playlist_id=id
                                refresh=Signal::from(set_list_version)
                                on_changed=bump
                                store=store.clone()
                            />
                        </section>

                        // 2. Compact player: now-playing metadata + current/next
                        //    lyric line. Sits right under the setlist so the
                        //    operator can glance up from a tap to see state.
                        <section class="live-section live-section-player">
                            <NowPlayingCard playlist_id=id store=store.clone() />
                            <LyricsScroller playlist_id=id store=store.clone() />
                        </section>

                        // 3. "Add songs" is tucked into a collapsible panel at
                        //    the bottom. Closed by default — during a live set
                        //    the operator rarely needs the catalog, and when
                        //    they do it's fine to scroll to the bottom + tap.
                        <section class="live-section live-section-add">
                            <button
                                class="live-add-toggle"
                                on:click=move |_| add_open.update(|o| *o = !*o)
                            >
                                {move || if add_open.get() {
                                    "▾ Add songs to set list"
                                } else {
                                    "▸ Add songs to set list"
                                }}
                            </button>
                            <div
                                class="live-add-body"
                                class:open=move || add_open.get()
                            >
                                <ImportUrlBox
                                    playlist_id=id
                                    on_imported=Callback::new(move |(_vid, _title)| {
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
