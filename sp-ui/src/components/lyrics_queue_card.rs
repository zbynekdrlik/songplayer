//! Card showing the lyrics pipeline queue state and controls.

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::store::{DashboardStore, LyricsQueueInfo};

#[component]
pub fn LyricsQueueCard() -> impl IntoView {
    let store = expect_context::<DashboardStore>();
    let queue = store.lyrics_queue;

    // Fetch initial state.
    spawn_local(async move {
        if let Ok(val) = api::get_lyrics_queue().await {
            if let (Some(b0), Some(b1), Some(b2), Some(pv)) = (
                val.get("bucket0_count").and_then(|v| v.as_i64()),
                val.get("bucket1_count").and_then(|v| v.as_i64()),
                val.get("bucket2_count").and_then(|v| v.as_i64()),
                val.get("pipeline_version").and_then(|v| v.as_u64()),
            ) {
                queue.set(Some(LyricsQueueInfo {
                    bucket0: b0,
                    bucket1: b1,
                    bucket2: b2,
                    pipeline_version: pv as u32,
                    processing: None,
                }));
            }
        }
    });

    let on_reprocess_all = move |_| {
        spawn_local(async move {
            let _ = api::post_reprocess_all_stale().await;
        });
    };
    let on_clear_manual = move |_| {
        spawn_local(async move {
            let _ = api::post_clear_manual_queue().await;
        });
    };

    view! {
        <div class="lyrics-queue-card">
            <h2>"Lyrics Pipeline"</h2>
            {move || match queue.get() {
                None => view! { <p>"Loading queue..."</p> }.into_any(),
                Some(q) => {
                    let proc_block = q.processing.as_ref().map(|p| {
                        let stage_label = match p.provider.as_ref() {
                            Some(prov) => format!("{} ({prov})", p.stage),
                            None => p.stage.clone(),
                        };
                        view! {
                            <div class="lyrics-processing">
                                <strong>"Currently processing: "</strong>
                                {format!("{} \u{2014} {}", p.song, p.artist)}
                                <div>"Stage: "{stage_label}</div>
                            </div>
                        }
                    });
                    view! {
                        <>
                            {proc_block}
                            <ul class="lyrics-queue-counts">
                                <li>"Manual: "<b>{q.bucket0}</b></li>
                                <li>"New: "<b>{q.bucket1}</b></li>
                                <li>
                                    "Stale: "<b>{q.bucket2}</b>
                                    <button on:click=on_reprocess_all>
                                        "Reprocess all stale"
                                    </button>
                                </li>
                            </ul>
                            <div class="lyrics-pipeline-version">
                                "Pipeline version: "<b>{q.pipeline_version}</b>
                                <button on:click=on_clear_manual>"Clear manual queue"</button>
                            </div>
                        </>
                    }
                    .into_any()
                }
            }}
        </div>
    }
}
