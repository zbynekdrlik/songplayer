//! `PresenterPayload` — request body for `PUT /api/stage` on the Presenter
//! stage-display API. Matches the spec exactly:
//!   - field names serialize camelCase: `currentText`, `nextText`, etc.
//!   - missing-on-the-wire fields default to "" server-side (not displayed)
//!
//! `currentGroup` / `nextGroup` are intentionally omitted — SongPlayer has
//! no notion of worship-team groups today. Follow-up can add them via
//! per-playlist or per-line metadata when bands start asking for colour
//! bars on the stage display.

use serde::Serialize;

/// Maximum characters per visual line on the Presenter stage display before
/// we force a line break. Many source lyrics are long narrative lines that
/// wrap awkwardly on a phone/tablet stage display. Splitting at word
/// boundaries around this width keeps each visual line readable at a
/// glance without redesigning the upstream lyrics pipeline.
pub const PRESENTER_WRAP_WIDTH: usize = 30;

/// Wrap `text` so no visual line exceeds `PRESENTER_WRAP_WIDTH` characters,
/// breaking at the last whitespace before the limit when possible. Existing
/// newlines in `text` are preserved — each pre-existing line is wrapped
/// independently. A word longer than the limit is left intact on its own
/// line (we never split mid-word; the display just renders it slightly
/// wider than ideal, which is still more readable than a mid-word break).
pub fn wrap_for_presenter(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(text.len() + 4);
    let mut first_chunk = true;
    for raw_line in text.split('\n') {
        if !first_chunk {
            out.push('\n');
        }
        first_chunk = false;
        out.push_str(&wrap_single_line(raw_line, PRESENTER_WRAP_WIDTH));
    }
    out
}

/// Greedy word-wrap for a single line. UTF-8 safe: we measure width in
/// Unicode scalar values (`.chars().count()`) rather than bytes so multi-
/// byte Slovak characters count as 1 each, matching what a reader sees.
fn wrap_single_line(line: &str, max: usize) -> String {
    let char_count = line.chars().count();
    if char_count <= max {
        return line.to_string();
    }
    let words: Vec<&str> = line.split_whitespace().collect();
    if words.is_empty() {
        return line.to_string();
    }

    let mut out = String::with_capacity(line.len() + 4);
    let mut cur_len = 0usize;
    for (i, word) in words.iter().enumerate() {
        let word_len = word.chars().count();
        let need = if cur_len == 0 {
            word_len
        } else {
            cur_len + 1 + word_len
        };
        if i > 0 && need > max {
            out.push('\n');
            cur_len = 0;
        } else if i > 0 {
            out.push(' ');
            cur_len += 1;
        }
        out.push_str(word);
        cur_len += word_len;
    }
    out
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PresenterPayload {
    pub current_text: String,
    pub next_text: String,
    pub current_song: String,
    pub next_song: String,
}

impl PresenterPayload {
    /// Four empty strings — clears the stage display on the Presenter side.
    pub fn empty() -> Self {
        Self {
            current_text: String::new(),
            next_text: String::new(),
            current_song: String::new(),
            next_song: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_with_camel_case_keys_matching_api_spec() {
        let p = PresenterPayload {
            current_text: "Haleluja, haleluja".to_string(),
            next_text: "Spievajte Hospodinovi".to_string(),
            current_song: "Haleluja".to_string(),
            next_song: "Spievajte".to_string(),
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["currentText"], "Haleluja, haleluja");
        assert_eq!(json["nextText"], "Spievajte Hospodinovi");
        assert_eq!(json["currentSong"], "Haleluja");
        assert_eq!(json["nextSong"], "Spievajte");
    }

    #[test]
    fn does_not_serialize_group_fields() {
        // currentGroup / nextGroup are intentionally NOT on this struct.
        // The Presenter API treats missing fields as empty, not displayed —
        // which is what we want until SongPlayer learns about groups.
        let p = PresenterPayload::empty();
        let json = serde_json::to_value(&p).unwrap();
        let obj = json.as_object().expect("object");
        assert!(
            obj.get("currentGroup").is_none(),
            "currentGroup must NOT appear in the serialized body (got: {json})"
        );
        assert!(
            obj.get("nextGroup").is_none(),
            "nextGroup must NOT appear in the serialized body (got: {json})"
        );
    }

    #[test]
    fn empty_returns_four_empty_strings() {
        let p = PresenterPayload::empty();
        assert!(p.current_text.is_empty());
        assert!(p.next_text.is_empty());
        assert!(p.current_song.is_empty());
        assert!(p.next_song.is_empty());
    }

    // ---- wrap_for_presenter tests -----------------------------------

    #[test]
    fn wrap_passes_through_short_text_unchanged() {
        let s = "Haleluja, haleluja";
        assert_eq!(s.chars().count(), 18);
        assert_eq!(wrap_for_presenter(s), s);
    }

    #[test]
    fn wrap_passes_through_text_at_exact_limit() {
        // 30 chars exactly — no break.
        let s = "a".repeat(30);
        assert_eq!(s.chars().count(), 30);
        assert_eq!(wrap_for_presenter(&s), s);
    }

    #[test]
    fn wrap_breaks_at_word_boundary_before_limit() {
        // "I want to hold my breath forever" is 32 chars — must break.
        let input = "I want to hold my breath forever";
        let wrapped = wrap_for_presenter(input);
        assert!(
            wrapped.contains('\n'),
            "expected a line break in: {wrapped:?}"
        );
        for line in wrapped.split('\n') {
            assert!(
                line.chars().count() <= PRESENTER_WRAP_WIDTH,
                "line `{line}` exceeds {PRESENTER_WRAP_WIDTH} chars"
            );
        }
        // No whitespace should have been dropped; re-joining with a space
        // gives back the normalized original.
        let rejoined: String = wrapped.replace('\n', " ");
        assert_eq!(rejoined, input);
    }

    #[test]
    fn wrap_handles_utf8_slovak_diacritics_as_single_chars() {
        // Slovak "ž", "ť" etc. are 2 bytes but 1 display char. A byte-based
        // wrapper would break too early here. This line is 25 Unicode chars,
        // so it must pass through untouched.
        let s = "Nedokážem pochopiť tvoju lás"; // 28 chars
        assert_eq!(s.chars().count(), 28);
        assert_eq!(wrap_for_presenter(s), s);
    }

    #[test]
    fn wrap_never_breaks_mid_word_even_if_word_is_too_long() {
        // 40-char single word. No whitespace to break at — emit as-is on
        // its own line (readable, just slightly wider than ideal).
        let long = "a".repeat(40);
        let wrapped = wrap_for_presenter(&long);
        assert_eq!(wrapped, long, "must not split a lone long word");
    }

    #[test]
    fn wrap_preserves_existing_newlines() {
        // Source text with explicit line breaks — each is wrapped
        // independently, and the existing break is preserved.
        let input = "short line\nanother short one";
        assert_eq!(wrap_for_presenter(input), input);
    }

    #[test]
    fn wrap_handles_empty_string() {
        assert_eq!(wrap_for_presenter(""), "");
    }

    /// Precise boundary test for the `need > max` comparison. Input is
    /// crafted so that after 5 five-char words + spaces (`cur_len = 29`),
    /// adding a 1-char word makes `need = 29 + 1 + 1 = 31 > 30` → break.
    ///
    /// Kills both `+` → `*` arithmetic mutants on line 62 (either mutation
    /// makes `need = 30` instead of `31`, which doesn't exceed `max`, so
    /// the break never happens → output has zero `\n`).
    /// Also kills `>` → `<` on line 64 (inverting makes almost every word
    /// break → many `\n`, not exactly 1).
    #[test]
    fn wrap_single_line_breaks_when_space_tips_over_max() {
        // "aaaaa bbbbb ccccc ddddd eeeee f" = 5+1+5+1+5+1+5+1+5+1+1 = 35 chars
        let input = "aaaaa bbbbb ccccc ddddd eeeee f";
        assert_eq!(input.chars().count(), 35);
        let wrapped = wrap_for_presenter(input);
        assert_eq!(
            wrapped.matches('\n').count(),
            1,
            "expected exactly 1 break in: {wrapped:?}"
        );
        // The break must be right before "f" — first line is the 29-char
        // prefix of five 5-char words with single spaces.
        let (first, rest) = wrapped.split_once('\n').expect("has one break");
        assert_eq!(first, "aaaaa bbbbb ccccc ddddd eeeee");
        assert_eq!(rest, "f");
    }

    /// Precise test for the `need > max` vs `need >= max` boundary. Input
    /// is crafted so the 6th word lands `need = cur_len + 1 + word_len =
    /// 24 + 1 + 5 = 30`, which is exactly `max`. Strict `>` must NOT
    /// break (first line ends up 30 chars). `>=` mutant would break here,
    /// producing a 24-char first line.
    ///
    /// We need a 7th word to force entry into the wrap loop (total char
    /// count must exceed max).
    #[test]
    fn wrap_single_line_first_line_fills_to_exactly_max() {
        // "aaaa bbbb cccc dddd eeee fffff g" = 4+1+4+1+4+1+4+1+4+1+5+1+1 = 32 chars
        let input = "aaaa bbbb cccc dddd eeee fffff g";
        assert_eq!(input.chars().count(), 32);
        let wrapped = wrap_for_presenter(input);
        let first_line = wrapped.split('\n').next().expect("has lines");
        assert_eq!(
            first_line.chars().count(),
            PRESENTER_WRAP_WIDTH,
            "first line must fill to exactly max=30 chars; got {first_line:?}"
        );
        assert_eq!(first_line, "aaaa bbbb cccc dddd eeee fffff");
    }

    /// Test for the `cur_len += word_len` accumulator. With `*=` mutant,
    /// `cur_len` grows multiplicatively instead of additively, producing
    /// a DIFFERENT number of breaks than real addition. Ten 5-char words
    /// with max=30 yields exactly ONE break in reality (break after
    /// word 5 at cur_len=29+1+5=35>30). The mutant produces more breaks
    /// because `cur_len *= 5` hits 30 faster.
    ///
    /// Asserts the exact output so any arithmetic change in the
    /// accumulator is caught.
    #[test]
    fn wrap_single_line_uses_additive_cur_len_increment() {
        // 10 × "hello" separated by spaces: 5 + 9*(1+5) = 5 + 54 = 59 chars
        let input = "hello hello hello hello hello hello hello hello hello hello";
        assert_eq!(input.chars().count(), 59);
        let wrapped = wrap_for_presenter(input);
        // Real `+=` trace: break happens after exactly 5 words (cur_len
        // reaches 29, 6th word pushes need to 35 > 30 → break). Words
        // 6-10 fit on the second line (cur_len reaches 29 again but
        // there's no 11th word to trigger another break).
        assert_eq!(
            wrapped, "hello hello hello hello hello\nhello hello hello hello hello",
            "additive increment must produce exactly 1 break after 5 words on each half"
        );
    }
}
