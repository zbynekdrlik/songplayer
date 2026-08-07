//! Shared metadata sanitization.
//!
//! Moved out of `gemini.rs` (#135) so the emoji sanitizer is a single
//! choke point every metadata path can share: `metadata::get_metadata`
//! applies it to whatever a provider returns AND to the regex-parser
//! fallback, so Gemini, Claude, and the fallback all ship clean text —
//! not just Gemini, which was the only path that called this internally
//! before.

/// Replace common emojis with text equivalents, then strip remaining non-text chars.
pub fn strip_emoji(s: &str) -> String {
    // Replace known emojis: hearts → "Love", others → remove
    let replaced = s
        .replace(
            [
                '\u{2764}',
                '\u{1F90D}',
                '\u{1F499}',
                '\u{1F49C}',
                '\u{2665}',
            ],
            "Love",
        )
        .replace(
            [
                '\u{1F525}',
                '\u{1F64F}',
                '\u{2728}',
                '\u{1F3B6}',
                '\u{1F3B5}',
            ],
            "",
        );
    // Strip any remaining non-text characters.
    // Keep: ASCII + Latin Extended (< 0x2600) and variation selectors (FE00-FE0F).
    // 0x00C0-0x024F (Latin Extended) is already covered by < 0x2600.
    replaced
        .chars()
        .filter(|c| {
            let cp = *c as u32;
            cp < 0x2600 || (0xFE00..=0xFE0F).contains(&cp)
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
//
// The bulk of this function's behavioral coverage lives in `gemini.rs`'s
// test module (`strip_emoji_replaces_hearts_with_love` and friends),
// which imports it from here — kept there per #135's brief so the RED/GREEN
// diff only touches import paths, not test bodies. This module keeps a
// minimal smoke test so `sanitize.rs` is self-verifying in isolation too.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_emoji_is_reachable_from_its_own_module() {
        assert_eq!(strip_emoji("Song \u{1F525} Title"), "Song Title");
        assert_eq!(strip_emoji("Yahweh We \u{1F90D} You"), "Yahweh We Love You");
    }
}
