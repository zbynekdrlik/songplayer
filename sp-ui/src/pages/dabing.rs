//! Dabing section page (#180). A paste-a-URL field for a priority add + the list
//! of dub-requested videos with their chain state and a Prehrať button. A 2 s
//! poll (owned here, like the NDI-health loop) refreshes `store.dabing`.

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::components::dabing_list::DabingList;
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

    let url = RwSignal::new(String::new());
    let status = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    // The Dabing playlist id, resolved from the first `/api/v1/dabing` payload.
    let dabing_pid = RwSignal::new(None::<i64>);

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
                if let Ok(v) = api::get_dabing().await {
                    if let Some(pid) = parse_dabing_pid(&v) {
                        let _ = dabing_pid.try_set(Some(pid));
                    }
                    if dabing.try_set(parse_dabing(&v)).is_some() {
                        break; // signal disposed
                    }
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
                        if let Some(pid) = parse_dabing_pid(&v) {
                            dabing_pid.set(Some(pid));
                        }
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
            // #194: the ONE shared player for the Dabing playlist output. The
            // rows' Prehrať below start a video on this same output, shown here.
            {move || match dabing_pid.get() {
                Some(id) => view! { <Player playlist_id=id /> }.into_any(),
                None => view! { <span></span> }.into_any(),
            }}
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
