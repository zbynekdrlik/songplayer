//! Modal showing detailed lyrics audit info + the song's lyric lines for one
//! song. #194 r3c: renders the shared `LyricsView` (scroll mode) for the lyric
//! lines; all text is Slovak.

use leptos::callback::Callable;
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::components::lyrics_view::{LyricsMode, LyricsView};
use crate::components::state_block::{StateBlock, StateKind};

#[component]
pub fn LyricsSongDetailModal(video_id: i64, on_close: Callback<()>) -> impl IntoView {
    let detail = RwSignal::new(None::<serde_json::Value>);

    spawn_local({
        let detail = detail;
        async move {
            if let Ok(val) = api::get_lyrics_song_detail(video_id).await {
                let _ = detail.try_set(Some(val));
            }
        }
    });

    let on_close_click = move |_| on_close.run(());

    view! {
        <div class="modal-backdrop" on:click=on_close_click>
            <div class="modal" on:click=|e: leptos::ev::MouseEvent| e.stop_propagation()>
                <button class="modal-close" on:click=on_close_click>"\u{00D7}"</button>
                {move || match detail.get() {
                    None => view! { <StateBlock kind=StateKind::Loading /> }.into_any(),
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
                                "Zdroj: "<code>{source}</code>" | Kvalita: "{quality}
                            </p>
                            // #194: the song's lyric lines via the shared LyricsView.
                            <LyricsView mode=LyricsMode::Scroll video_id=video_id />
                            <details>
                                <summary>"Surový audit"</summary>
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
