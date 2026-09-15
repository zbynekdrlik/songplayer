//! Karaoke panel showing current lyrics line with word-level highlighting.

use crate::store::NowPlayingInfo;
use leptos::prelude::*;

/// A non-breaking space placeholder so an empty line keeps its height and the
/// panel never collapses (#163 — the block must not resize the card).
const NBSP: &str = "\u{00A0}";

#[component]
pub fn KaraokePanel(info: NowPlayingInfo) -> impl IntoView {
    // #163: the panel is ALWAYS rendered with a reserved fixed height (see
    // `.karaoke-panel` in style.css), and all four line slots are always present
    // — an empty slot falls back to a non-breaking space. So the content swaps
    // INSIDE a fixed box and the card below it never jumps up and down when
    // lyrics pause. Previously the whole panel was conditionally rendered and
    // disappeared between lines, collapsing the card (the owner's complaint).
    let prev_line = info.prev_line_en.clone().unwrap_or_default();
    let next_line = info.next_line_en.clone().unwrap_or_default();
    let current_en = info.line_en.clone().unwrap_or_default();
    let current_sk = info.line_sk.clone().unwrap_or_default();
    let active_idx = info.active_word_index.unwrap_or(0);

    let words: Vec<String> = current_en.split_whitespace().map(String::from).collect();

    view! {
        <div class="karaoke-panel">
            <div class="karaoke-line karaoke-dim">
                {if prev_line.is_empty() { NBSP.to_string() } else { prev_line }}
            </div>
            <div class="karaoke-line karaoke-current">
                {if words.is_empty() {
                    view! { <span class="karaoke-word">{NBSP}</span> }.into_any()
                } else {
                    words
                        .into_iter()
                        .enumerate()
                        .map(|(i, word)| {
                            let class = if i < active_idx {
                                "karaoke-word karaoke-word-past"
                            } else if i == active_idx {
                                "karaoke-word karaoke-word-active"
                            } else {
                                "karaoke-word karaoke-word-future"
                            };
                            view! { <span class=class>{word}{" "}</span> }
                        })
                        .collect_view()
                        .into_any()
                }}
            </div>
            <div class="karaoke-line karaoke-sk">
                {if current_sk.is_empty() { NBSP.to_string() } else { current_sk }}
            </div>
            <div class="karaoke-line karaoke-dim">
                {if next_line.is_empty() { NBSP.to_string() } else { next_line }}
            </div>
        </div>
    }
}
