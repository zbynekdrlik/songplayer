//! Download progress display.

use leptos::prelude::*;

use crate::store::DashboardStore;

#[component]
pub fn DownloadQueue() -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");

    view! {
        <div class="download-queue">
            {move || {
                let queue = store.download_queue.get();
                if queue.is_empty() {
                    view! { <span></span> }.into_any()
                } else {
                    view! {
                        <h2>"Downloads"</h2>
                        <div class="queue-items">
                            <For
                                each=move || store.download_queue.get()
                                key=|d| d.youtube_id.clone()
                                children=|item| {
                                    view! {
                                        <div class="download-item">
                                            <span class="dl-title">{item.title.clone()}</span>
                                            <span class="dl-stage">{item.stage.clone()}</span>
                                            <div class="progress-bar">
                                                <div
                                                    class="progress-fill"
                                                    style:width=format!("{:.0}%", item.progress_pct)
                                                ></div>
                                            </div>
                                            <span class="dl-pct">{format!("{:.0}%", item.progress_pct)}</span>
                                        </div>
                                    }
                                }
                            />
                        </div>
                    }
                        .into_any()
                }
            }}
        </div>
    }
}
