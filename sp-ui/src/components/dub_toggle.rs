//! Per-row "Dabing" toggle (#180). Placed in every video row (any playlist); a
//! flip PATCHes `/api/v1/videos/{id}/dub {requested}` — requesting queues the
//! dubbing chain with priority. Optimistic local state; reverts on API error.

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;

#[component]
pub fn DubToggle(video_id: i64, initial: bool) -> impl IntoView {
    let requested = RwSignal::new(initial);
    let busy = RwSignal::new(false);

    let flip = move || {
        if busy.get_untracked() {
            return;
        }
        let next = !requested.get_untracked();
        busy.set(true);
        requested.set(next); // optimistic
        spawn_local(async move {
            if api::patch_dub(video_id, next).await.is_err() {
                requested.set(!next); // revert on failure
            }
            busy.set(false);
        });
    };

    view! {
        <label class="dub-toggle" title="Zaradiť na dabing">
            <input
                type="checkbox"
                data-testid="dub-toggle"
                prop:checked=move || requested.get()
                prop:disabled=move || busy.get()
                on:change=move |_| flip()
            />
            "Dabing"
        </label>
    }
}
