//! A single row in the lyrics song list — the shared `SongRow` (#194) with the
//! text chip (source + quality folded into its tooltip) and the lyrics actions
//! (detail / reprocess / translation gender / reference reject).

use leptos::callback::Callable;
use leptos::prelude::*;
use leptos::task::spawn_local;
use sp_core::status_chip::text_chip;

use crate::api;
use crate::components::song_row::SongRow;
use crate::components::status_chips::ChipView;
use crate::store::{DashboardStore, LyricsSongEntry, ReprocessOutcome};

#[component]
pub fn LyricsSongRow(entry: LyricsSongEntry, on_details: Callback<i64>) -> impl IntoView {
    let store = expect_context::<DashboardStore>();
    let last_reprocess = store.last_reprocess;
    // #152: per-song SK translation gender override. Cycles auto → ♂ → ♀ →
    // auto; each click PATCHes and optimistically flips the glyph.
    let gender = RwSignal::new(entry.translation_gender.clone());

    let video_id = entry.video_id;
    let title = entry
        .song
        .clone()
        .unwrap_or_else(|| entry.youtube_id.clone());
    let artist = entry.artist.clone().unwrap_or_default();

    // The text chip carries the whole lyrics state; source + quality fold into
    // its tooltip so the row no longer needs a separate source/quality chip.
    let source_text = entry.source.clone().unwrap_or_else(|| "—".into());
    let mut tip = format!("zdroj: {source_text}");
    if let Some(q) = entry.quality_score {
        tip.push_str(&format!(", q={q:.2}"));
    }
    let chips = vec![ChipView::with_tip(
        text_chip(entry.has_lyrics, entry.lyrics_reference, entry.is_stale),
        tip,
    )];

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

    let show_reject = entry.lyrics_reference;

    view! {
        <SongRow video_id=video_id title=title artist=artist chips=chips>
            <button type="button" class="song-row-btn" on:click=on_details_click>
                "Detail"
            </button>
            <button
                type="button"
                class="song-row-btn"
                title="Znova spracovať text"
                on:click=on_reprocess
            >
                "Preprac."
            </button>
            <button
                type="button"
                class="song-row-btn translation-gender-btn"
                title="Rod prekladu (prvá osoba)"
                on:click=on_gender
            >
                {gender_glyph}
            </button>
            {show_reject
                .then(|| {
                    // #142: owner flags a starred song as wrong. Prompt for a
                    // short note, POST it to the feedback endpoint.
                    view! {
                        <button
                            type="button"
                            class="song-row-btn reference-reject-btn"
                            title="Referenčný text nesedí"
                            on:click=move |_| {
                                let note = web_sys::window()
                                    .and_then(|w| {
                                        w.prompt_with_message("Prečo referenčný text nesedí?").ok()
                                    })
                                    .flatten();
                                let Some(note) = note.filter(|n| !n.trim().is_empty()) else {
                                    return;
                                };
                                spawn_local(async move {
                                    let _ = api::post_reference_feedback(video_id, &note).await;
                                });
                            }
                        >
                            "Nesedí"
                        </button>
                    }
                })}
        </SongRow>
    }
}
