//! A single row in the lyrics song list.

use leptos::callback::Callable;
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::store::{DashboardStore, LyricsSongEntry, ReprocessOutcome};

#[component]
pub fn LyricsSongRow(
    entry: LyricsSongEntry,
    on_details: Callback<i64>,
) -> impl IntoView {
    let store = expect_context::<DashboardStore>();
    let last_reprocess = store.last_reprocess;
    // #142: local reactive copy of the ★ reference flag. Seeded from the
    // fetched entry; cleared optimistically the moment "Nesedí" feedback
    // is recorded server-side, so the star disappears without needing a
    // full re-fetch of the (non-reactive) parent song list.
    let is_reference = RwSignal::new(entry.lyrics_reference);
    // #152: per-song SK translation gender override. Cycles auto → ♂ → ♀ →
    // auto; each click PATCHes and optimistically flips the glyph.
    let gender = RwSignal::new(entry.translation_gender.clone());
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
            if let Ok(v) = api::post_reprocess_videos(&[video_id]).await {
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
    let on_details_click = move |_| on_details.run(video_id);

    // #152: ♂♀ = auto (masculine default), ♂ = forced masculine, ♀ = feminine.
    let gender_glyph = move || match gender.get().as_deref() {
        Some("m") => "\u{2642}",
        Some("f") => "\u{2640}",
        _ => "\u{2642}\u{2640}",
    };
    let on_gender = move |_| {
        let next: Option<&'static str> = match gender.get_untracked().as_deref() {
            None => Some("m"),
            Some("m") => Some("f"),
            _ => None,
        };
        spawn_local(async move {
            if api::patch_translation_gender(video_id, next).await.is_ok() {
                gender.set(next.map(|s| s.to_string()));
            }
        });
    };

    view! {
        <div class={format!("lyrics-song-row {status_class}")}>
            <span class="status-icon">{status_icon}</span>
            <span class="song-display">{display}</span>
            {move || {
                is_reference
                    .get()
                    .then(|| view! { <span class="reference-badge">"\u{2605}"</span> })
            }}
            <span class="source-chip">{source_text}</span>
            <span class="quality-text">{quality_text}</span>
            <button on:click=on_details_click>"Details"</button>
            <button on:click=on_reprocess>"Reprocess"</button>
            <button
                class="translation-gender-btn"
                title="Rod prekladu (prvá osoba)"
                on:click=on_gender
            >
                {gender_glyph}
            </button>
            {move || {
                is_reference
                    .get()
                    .then(|| {
                        // #142: owner flags a starred song as wrong. Prompt
                        // for a short note, POST it to the feedback
                        // endpoint, and clear the star on success.
                        view! {
                            <button
                                class="reference-reject-btn"
                                on:click=move |_| {
                                    let note = web_sys::window()
                                        .and_then(|w| {
                                            w.prompt_with_message("Prečo referenčný text nesedí?")
                                                .ok()
                                        })
                                        .flatten();
                                    let Some(note) = note.filter(|n| !n.trim().is_empty()) else {
                                        return;
                                    };
                                    spawn_local(async move {
                                        if api::post_reference_feedback(video_id, &note)
                                            .await
                                            .is_ok()
                                        {
                                            is_reference.set(false);
                                        }
                                    });
                                }
                            >
                                "Nesedí"
                            </button>
                        }
                    })
            }}
        </div>
    }
}
