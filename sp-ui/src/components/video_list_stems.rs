//! #177: karaoke-stems marker + "len so stemami" filter for the video list.
//! Sibling of `video_list.rs` (cohesion + the 1000-line cap). The marker reads
//! the additive per-video `stems_state` on the videos payload; the filter keeps
//! only stems-ready songs so the operator can pick one to try karaoke with.

use leptos::prelude::*;
use sp_core::models::Video;

/// Glyph + tooltip for a video's `stems_state` (mirrors the karaoke panel's
/// header vocabulary). An unknown / absent state renders nothing.
pub fn stems_glyph(state: Option<&str>) -> (&'static str, &'static str) {
    match state {
        Some("ready") => ("●", "Stemy pripravené — karaoke funguje"),
        Some("processing") => ("⚙", "Stemy sa spracúvajú"),
        Some("queued") => ("⏳", "Stemy vo fronte"),
        Some("unavailable") => ("—", "Stemy nedostupné (pridlhá / bez vokálov)"),
        Some("failed") => ("✖", "Spracovanie stemov zlyhalo"),
        _ => ("", ""),
    }
}

/// Filter predicate for "len so stemami": a song passes when the filter is off,
/// or when its stems are ready on disk.
pub fn passes_stems_filter(only_stems: bool, v: &Video) -> bool {
    !only_stems || v.stems_state.as_deref() == Some("ready")
}

/// A small stems-state marker cell for one video row.
#[component]
pub fn StemsMarker(state: Option<String>) -> impl IntoView {
    let (glyph, tip) = stems_glyph(state.as_deref());
    view! {
        <span
            class="video-list-stems-marker"
            data-testid="video-list-stems-marker"
            data-stems-state=state.clone().unwrap_or_default()
            title=tip
        >
            {glyph}
        </span>
    }
}
