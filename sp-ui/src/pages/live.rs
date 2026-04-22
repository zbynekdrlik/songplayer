//! /live page: two-column catalog + set-list for the custom ytlive playlist.

use leptos::prelude::*;

use crate::api;
use crate::components::import_url_box::ImportUrlBox;
use crate::components::live_catalog::LiveCatalog;
use crate::components::live_setlist::LiveSetList;

#[component]
pub fn LivePage() -> impl IntoView {
    let ytlive_id = RwSignal::new(None::<i64>);
    let set_list_version = RwSignal::new(0u64);
    let error_msg = RwSignal::new(String::new());

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
                        <ImportUrlBox
                            playlist_id=id
                            on_imported=Callback::new(move |(_vid, _title)| {
                                set_list_version.update(|v| *v += 1);
                            })
                        />
                        <div class="live-page-grid">
                            <LiveCatalog
                                target_playlist_id=id
                                _set_list_version=Signal::from(set_list_version)
                                on_added=bump_after_add
                            />
                            <LiveSetList
                                playlist_id=id
                                refresh=Signal::from(set_list_version)
                                on_changed=bump
                            />
                        </div>
                    </>
                }.into_any(),
            }}
        </div>
    }
}
