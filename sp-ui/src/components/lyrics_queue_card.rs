//! Card showing the lyrics pipeline queue state and controls.

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::store::{DashboardStore, LyricsQueueInfo, ReprocessOutcome};

#[component]
pub fn LyricsQueueCard() -> impl IntoView {
    let store = expect_context::<DashboardStore>();
    let queue = store.lyrics_queue;
    let last_reprocess = store.last_reprocess;

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
            if let Ok(v) = api::post_reprocess_all_stale().await {
                let queued = v.get("queued").and_then(|x| x.as_i64()).unwrap_or(0);
                let blocked = v
                    .get("blocked_by_asr_gap")
                    .and_then(|x| x.as_i64())
                    .unwrap_or(0);
                last_reprocess.set(Some(ReprocessOutcome {
                    queued,
                    blocked_by_asr_gap: blocked,
                }));
            }
        });
    };
    let on_clear_manual = move |_| {
        spawn_local(async move {
            let _ = api::post_clear_manual_queue().await;
        });
    };

    // #98: surface `blocked_by_asr_gap` from the reprocess response so
    // the operator can tell when N rows in the request set are parked
    // at `lyrics_source='asr_gap'`. Bumping LYRICS_PIPELINE_VERSION is
    // the only way to retry them (per regression-test-first + the
    // feedback_no_bump_until_proven memory: user-authoritative gate).
    let banner = move || {
        last_reprocess.get().and_then(|r| {
            if r.blocked_by_asr_gap > 0 {
                let msg = format!(
                    "{} of {} blocked by asr_gap — bump LYRICS_PIPELINE_VERSION to retry",
                    r.blocked_by_asr_gap,
                    r.queued + r.blocked_by_asr_gap,
                );
                Some(view! {
                    <div class="reprocess-asr-gap-banner">{msg}</div>
                })
            } else {
                None
            }
        })
    };

    view! {
        <div class="lyrics-queue-card">
            <h2>"Lyrics Pipeline"</h2>
            {banner}
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
