//! Modal showing detailed lyrics audit information for a single song.

use leptos::callback::Callable;
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;

#[component]
pub fn LyricsSongDetailModal(video_id: i64, on_close: Callback<()>) -> impl IntoView {
    let detail = RwSignal::new(None::<serde_json::Value>);

    spawn_local({
        let detail = detail;
        async move {
            if let Ok(val) = api::get_lyrics_song_detail(video_id).await {
                detail.set(Some(val));
            }
        }
    });

    let on_close_click = move |_| on_close.run(());

    view! {
        <div class="modal-backdrop" on:click=on_close_click>
            <div class="modal" on:click=|e: leptos::ev::MouseEvent| e.stop_propagation()>
                <button class="modal-close" on:click=on_close_click>"\u{00D7}"</button>
                {move || match detail.get() {
                    None => view! { <p>"Loading..."</p> }.into_any(),
                    Some(d) => {
                        let audit_pretty = d
                            .get("audit_json")
                            .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
                            .unwrap_or_default();
                        let li = d.get("list_item").cloned().unwrap_or_default();
                        let song = li
                            .get("song")
                            .and_then(|v| v.as_str())
                            .unwrap_or("\u{2014}")
                            .to_string();
                        let artist = li
                            .get("artist")
                            .and_then(|v| v.as_str())
                            .unwrap_or("\u{2014}")
                            .to_string();
                        let source = li
                            .get("source")
                            .and_then(|v| v.as_str())
                            .unwrap_or("\u{2014}")
                            .to_string();
                        let quality = li
                            .get("quality_score")
                            .and_then(|v| v.as_f64())
                            .map(|q| format!("{q:.2}"))
                            .unwrap_or_else(|| "\u{2014}".into());
                        view! {
                            <h2>{song}" \u{2014} "{artist}</h2>
                            <p>
                                "Source: "<code>{source}</code>" | Quality: "{quality}
                            </p>
                            <details>
                                <summary>"Raw audit log"</summary>
                                <pre>{audit_pretty}</pre>
                            </details>
                        }
                        .into_any()
                    }
                }}
            </div>
        </div>
    }
}
