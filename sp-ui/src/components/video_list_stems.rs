//! #177: the "len so stemami" filter predicate for the Dashboard song list.
//!
//! The per-song stems STATE is now rendered by the shared `StatusChips`
//! (`sp_core::status_chip::stems_chip`) — the old bare-glyph table + `StemsMarker`
//! were deleted in #194 in favour of the one status vocabulary. Only the filter
//! predicate stays here (it drives which rows the list shows).

use sp_core::models::Video;

/// Filter predicate for "len so stemami": a song passes when the filter is off,
/// or when its stems are ready on disk.
pub fn passes_stems_filter(only_stems: bool, v: &Video) -> bool {
    !only_stems || v.stems_state.as_deref() == Some("ready")
}
