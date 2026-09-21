//! Dabing section page (#180). The shared paste-URL `ImportBox` for a priority
//! add + the shared `Player` for the Dabing output + the list of dub-requested
//! videos. #184: the `/api/v1/dabing` 2 s poll now lives in `App`
//! (`store.dabing` + the Dabing playlist id exist on every page); this page just
//! reads them, plus a one-shot refresh right after an import.

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::components::dabing_list::DabingList;
use crate::components::import_box::{ImportBox, ImportTarget};
use crate::components::player::Player;
use crate::store::{DashboardStore, DubRow};

/// Parse `{playlist_id, videos:[…]}` into the DubRow list.
fn parse_dabing(v: &serde_json::Value) -> Vec<DubRow> {
    v.get("videos")
        .and_then(|a| serde_json::from_value::<Vec<DubRow>>(a.clone()).ok())
        .unwrap_or_default()
}

/// The Dabing playlist id carried by the `/api/v1/dabing` payload — needed so
/// the shared `Player` can drive that playlist's output (#194).
fn parse_dabing_pid(v: &serde_json::Value) -> Option<i64> {
    v.get("playlist_id").and_then(|p| p.as_i64())
}

#[component]
pub fn DabingPage() -> impl IntoView {
    let store = expect_context::<DashboardStore>();

    // #184: the `/api/v1/dabing` poll now lives in `App`, so `store.dabing` and
    // the Dabing playlist id exist on EVERY page (the shared Player's mixer slot
    // needs the dub row on the Dashboard / Live too). The page just reads them.
    let dabing_pid = store.dabing_playlist_id;

    // #194 hotfix: mount the shared `Player` through a `Memo` so even if the id
    // signal is ever fired with an unchanged value, the `Player` is created ONCE
    // and a position tick can never re-create it (the preview + mixer stay live).
    let player_pid = Memo::new(move |_| dabing_pid.get());

    // Refresh immediately after an import so the queued row shows without waiting
    // for the next poll tick. Writes go through `try_set` — this async runs after
    // the button click and the page may have been navigated away (sp-ui-frontend).
    let on_imported = Callback::new(move |_: (i64, String)| {
        spawn_local(async move {
            if let Ok(v) = api::get_dabing().await {
                if let Some(pid) = parse_dabing_pid(&v)
                    && dabing_pid.try_get_untracked().flatten() != Some(pid)
                {
                    let _ = dabing_pid.try_set(Some(pid));
                }
                let _ = store.dabing.try_set(parse_dabing(&v));
            }
        });
    });

    view! {
        <div class="dabing-page">
            <h2>"Dabing"</h2>
            // #194: the ONE shared player for the Dabing playlist output. The
            // rows' play action below start a video on this same output, shown here.
            {move || match player_pid.get() {
                Some(id) => view! { <Player playlist_id=id /> }.into_any(),
                None => view! { <span></span> }.into_any(),
            }}
            <ImportBox target=ImportTarget::Dabing on_imported=on_imported />
            <DabingList />
        </div>
    }
}
