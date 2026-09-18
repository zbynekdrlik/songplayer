//! Dabing section page (#180). A paste-a-URL field for a priority add + the list
//! of dub-requested videos with their chain state and a Prehrať button. A 2 s
//! poll (owned here, like the NDI-health loop) refreshes `store.dabing`.

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::components::dabing_list::DabingList;
use crate::store::{DashboardStore, DubRow};

/// Parse `{playlist_id, videos:[…]}` into the DubRow list.
fn parse_dabing(v: &serde_json::Value) -> Vec<DubRow> {
    v.get("videos")
        .and_then(|a| serde_json::from_value::<Vec<DubRow>>(a.clone()).ok())
        .unwrap_or_default()
}

#[component]
pub fn DabingPage() -> impl IntoView {
    let store = expect_context::<DashboardStore>();
    let dabing = store.dabing;

    let url = RwSignal::new(String::new());
    let status = RwSignal::new(String::new());
    let busy = RwSignal::new(false);

    // One initial fetch + a 2 s poll loop, cancelled on unmount. `cancelled` is
    // page-owned, so a wake after navigation must use `try_get_untracked`
    // (None on a disposed signal) and stop rather than panic (sp-ui-frontend.md).
    let cancelled = RwSignal::new(false);
    on_cleanup(move || cancelled.set(true));
    let _poll = Effect::new(move |_| {
        spawn_local(async move {
            loop {
                if cancelled.try_get_untracked() != Some(false) {
                    break;
                }
                if let Ok(v) = api::get_dabing().await
                    && dabing.try_set(parse_dabing(&v)).is_some()
                {
                    break; // signal disposed
                }
                gloo_timers::future::TimeoutFuture::new(2_000).await;
            }
        });
    });

    let submit = move || {
        let trimmed = url.get_untracked().trim().to_string();
        if trimmed.is_empty() {
            status.set("Vlož najprv URL videa".to_string());
            return;
        }
        busy.set(true);
        status.set(String::new());
        spawn_local(async move {
            match api::import_dabing(trimmed).await {
                Ok(resp) => {
                    status.set(format!("pridané: {}", resp.title));
                    url.set(String::new());
                    // Refresh immediately so the queued row shows without waiting
                    // for the next poll tick.
                    if let Ok(v) = api::get_dabing().await {
                        dabing.set(parse_dabing(&v));
                    }
                }
                Err(e) => status.set(format!("import zlyhal: {e}")),
            }
            busy.set(false);
        });
    };

    view! {
        <div class="dabing-page">
            <h2>"Dabing"</h2>
            <div class="dabing-import import-url-box">
                <input
                    type="text"
                    class="import-url-input"
                    data-testid="dabing-import-input"
                    placeholder="Vlož URL videa (napr. https://youtu.be/…)"
                    prop:value=move || url.get()
                    on:input=move |ev| url.set(event_target_value(&ev))
                    on:keydown=move |ev| {
                        if ev.key() == "Enter" && !busy.get_untracked() {
                            submit();
                        }
                    }
                    prop:disabled=move || busy.get()
                />
                <button
                    class="import-url-btn"
                    data-testid="dabing-import-btn"
                    on:click=move |_| submit()
                    prop:disabled=move || busy.get()
                >
                    {move || if busy.get() { "Pridávam…" } else { "Pridať" }}
                </button>
                <div class="import-url-status">{move || status.get()}</div>
            </div>
            <DabingList />
        </div>
    }
}
