//! Karaoke panel showing current lyrics line with word-level highlighting.

use leptos::prelude::*;
use crate::store::NowPlayingInfo;

#[component]
pub fn KaraokePanel(info: NowPlayingInfo) -> impl IntoView {
    let has_lyrics = info.line_en.is_some()
        || info.prev_line_en.is_some()
        || info.next_line_en.is_some();

    if !has_lyrics {
        return view! { }.into_any();
    }

    let prev_line = info.prev_line_en.clone().unwrap_or_default();
    let next_line = info.next_line_en.clone().unwrap_or_default();
    let current_en = info.line_en.clone().unwrap_or_default();
    let current_sk = info.line_sk.clone().unwrap_or_default();
    let active_idx = info.active_word_index.unwrap_or(0);

    let words: Vec<String> = current_en.split_whitespace().map(String::from).collect();

    view! {
        <div class="karaoke-panel">
            {if !prev_line.is_empty() {
                view! { <div class="karaoke-line karaoke-dim">{prev_line}</div> }.into_any()
            } else {
                view! { }.into_any()
            }}
            <div class="karaoke-line karaoke-current">
                {words.into_iter().enumerate().map(|(i, word)| {
                    let class = if i < active_idx {
                        "karaoke-word karaoke-word-past"
                    } else if i == active_idx {
                        "karaoke-word karaoke-word-active"
                    } else {
                        "karaoke-word karaoke-word-future"
                    };
                    view! { <span class=class>{word}{" "}</span> }
                }).collect_view()}
            </div>
            {if !current_sk.is_empty() {
                view! { <div class="karaoke-line karaoke-sk">{current_sk}</div> }.into_any()
            } else {
                view! { }.into_any()
            }}
            {if !next_line.is_empty() {
                view! { <div class="karaoke-line karaoke-dim">{next_line}</div> }.into_any()
            } else {
                view! { }.into_any()
            }}
        </div>
    }.into_any()
}
