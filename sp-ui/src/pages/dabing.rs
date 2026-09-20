//! Dabing section page (#180). The shared paste-URL `ImportBox` for a priority
//! add + the shared `Player` for the Dabing output + the list of dub-requested
//! videos. A 2 s poll (owned here, like the NDI-health loop) refreshes
//! `store.dabing`.

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
    let dabing = store.dabing;

    // The Dabing playlist id, resolved from the first `/api/v1/dabing` payload.
    let dabing_pid = RwSignal::new(None::<i64>);

    // One initial fetch + a 2 s poll loop, cancelled on unmount. `cancelled` is
    // page-owned, so a wake after navigation must use `try_get_untracked`
    // (None on a disposed signal) and stop rather than panic (sp-ui-frontend.md).
    let cancelled = RwSignal::new(false);
    on_cleanup(move || cancelled.set(true));
    // #194 r3b: the once-hand-rolled 2 s loop now goes through the ONE shared
    // `store::poll_value` helper. `apply` parses the `{playlist_id, videos:[…]}`
    // payload into both signals and returns `true` (stop) when `dabing` is
    // disposed — same disposal discipline as the other polls, in one place.
    let _poll = Effect::new(move |_| {
        crate::store::poll_value("/api/v1/dabing", 2_000, cancelled, move |v| {
            if let Some(pid) = parse_dabing_pid(&v) {
                // #194 hotfix: only SET when the id actually CHANGES. `RwSignal::
                // set` fires subscribers unconditionally, so re-setting the same
                // id every 2 s tick re-ran the `match dabing_pid.get()` view below
                // and re-created the whole `<Player>` (tearing down its preview
                // WebSocket right after it opened, and resetting a mid-drag mixer).
                if dabing_pid.try_get_untracked().flatten() != Some(pid) {
                    let _ = dabing_pid.try_set(Some(pid));
                }
            }
            dabing.try_set(parse_dabing(&v)).is_some()
        });
    });

    // #194 hotfix: mount the shared `Player` through a `Memo` so even if the id
    // signal is ever fired with an unchanged value, the `Player` is created ONCE
    // and a position tick can never re-create it (the preview + mixer stay live).
    let player_pid = Memo::new(move |_| dabing_pid.get());

    // Refresh immediately after an import so the queued row shows without waiting
    // for the next poll tick.
    let on_imported = Callback::new(move |_: (i64, String)| {
        spawn_local(async move {
            if let Ok(v) = api::get_dabing().await {
                if let Some(pid) = parse_dabing_pid(&v) {
                    // Same change-gate as the poll — never re-create the Player.
                    // `try_get_untracked` (not `get_untracked`): this async runs
                    // after the button click and the page may have been navigated
                    // away, disposing the signal — a plain read would then panic
                    // (sp-ui-frontend.md); `flatten()` folds disposed → None.
                    if dabing_pid.try_get_untracked().flatten() != Some(pid) {
                        dabing_pid.set(Some(pid));
                    }
                }
                dabing.set(parse_dabing(&v));
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
