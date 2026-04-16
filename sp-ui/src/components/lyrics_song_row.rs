//! A single row in the lyrics song list.

use leptos::callback::Callable;
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::store::LyricsSongEntry;

#[component]
pub fn LyricsSongRow(
    entry: LyricsSongEntry,
    on_details: Callback<i64>,
) -> impl IntoView {
    let status_class = if !entry.has_lyrics {
        "status-none"
    } else if entry.is_stale {
        "status-stale"
    } else if entry.quality_score.map(|q| q < 0.5).unwrap_or(false) {
        "status-warn"
    } else {
        "status-ok"
    };
    let status_icon = match status_class {
        "status-ok" => "\u{25CF}",
        "status-stale" => "\u{25CF}",
        "status-warn" => "\u{26A0}",
        _ => "\u{2717}",
    };

    let display = format!(
        "{} \u{2014} {}",
        entry.song.clone().unwrap_or_else(|| entry.youtube_id.clone()),
        entry.artist.clone().unwrap_or_default()
    );
    let source_text = entry.source.clone().unwrap_or_else(|| "\u{2014}".into());
    let quality_text = entry
        .quality_score
        .map(|q| format!("q={q:.2}"))
        .unwrap_or_default();
    let video_id = entry.video_id;

    let on_reprocess = move |_| {
        spawn_local(async move {
            let _ = api::post_reprocess_videos(&[video_id]).await;
        });
    };
    let on_details_click = move |_| on_details.run(video_id);

    view! {
        <div class={format!("lyrics-song-row {status_class}")}>
            <span class="status-icon">{status_icon}</span>
            <span class="song-display">{display}</span>
            <span class="source-chip">{source_text}</span>
            <span class="quality-text">{quality_text}</span>
            <button on:click=on_details_click>"Details"</button>
            <button on:click=on_reprocess>"Reprocess"</button>
        </div>
    }
}
