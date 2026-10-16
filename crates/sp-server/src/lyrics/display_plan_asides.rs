//! #280: the parenthesised asides the wall and the Presenter leave out.
//!
//! The ★-tier text often comes from Genius, which writes ad-libs, backing
//! vocals and the leader's calls in parentheses: `Hallelu (Hey, oh,
//! hallelujah), hallelujah (I have been redeemed, yeah)`. Shown as stored they
//! made wall lines of up to 84 chars (song 361, owner 16.10.2026). The display
//! plan reads each source line through [`without_asides`] before it groups
//! them, so the shorter text is what `fits` measures and what the wall shows.
//! Display-only: the stored JSON, the karaoke view and the pipeline are
//! untouched.

use sp_core::lyrics::LyricsLine;

/// A word made only of these is glued to the word before it when the
/// words are joined again (`clean (Oh), yeah` → `clean, yeah`), and dropped
/// at the start of a line (`(Oh), yeah` → `yeah`). The interjection rule
/// reads the same marks as the ones that separate an edge run.
pub(super) const GLUED: [char; 7] = [',', '.', ';', ':', '!', '?', '…'];

/// `line` as the display plan reads it: its English and its Slovak each
/// through [`drop_asides`]; times and words unchanged.
pub(super) fn without_asides(line: &LyricsLine) -> LyricsLine {
    LyricsLine {
        en: drop_asides(&line.en),
        sk: line.sk.as_deref().map(drop_asides),
        ..line.clone()
    }
}

/// `text` without its parenthesised asides. Every balanced `( … )` goes,
/// nested ones included, and the words left are joined again
/// ([`join_words`]). A text with no parenthesis is returned as it is, and so
/// is one whose parentheses do not pair up. A text that is ONLY asides
/// (`(Hallelujah)`, sung by everyone) keeps its words without the brackets:
/// the wall never blanks a line the room sings.
fn drop_asides(text: &str) -> String {
    if !text.contains(['(', ')']) {
        return text.to_string();
    }
    let Some(outside) = outside_parentheses(text) else {
        return text.to_string();
    };
    match join_words(&outside) {
        joined if joined.is_empty() => without_brackets(text),
        joined => joined,
    }
}

/// `line` with only its brackets gone, every word kept (#280 part 2,
/// review round 1): a line whose words outside its asides are only
/// interjections is read so (`Oh (Hallelujah)` → `Oh Hallelujah`), as a
/// line of only asides keeps its words: the wall never blanks a sung aside.
pub(super) fn unbracketed(line: &LyricsLine) -> LyricsLine {
    LyricsLine {
        en: without_brackets(&line.en),
        sk: line.sk.as_deref().map(without_brackets),
        ..line.clone()
    }
}

/// The words of `text` with its brackets gone, joined again
/// ([`join_words`]).
fn without_brackets(text: &str) -> String {
    join_words(&text.replace(['(', ')'], " "))
}

/// The chars of `text` outside every parenthesis, a space where an aside
/// was (so the words on its two sides never run together); `None` when a
/// `)` closes nothing or a `(` is never closed.
fn outside_parentheses(text: &str) -> Option<String> {
    let mut outside = String::with_capacity(text.len());
    let mut depth = 0usize;
    for c in text.chars() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    outside.push(' ');
                }
            }
            _ if depth == 0 => outside.push(c),
            _ => {}
        }
    }
    (depth == 0).then_some(outside)
}

/// The words of `text` joined with one space, a word of [`GLUED`] chars
/// alone glued to the word before it, and dropped when nothing comes
/// before it. The interjection rule reads a line's words through it too, so
/// a lone mark never stops an edge run (`Oh , yeah` reads as `Oh, yeah`).
pub(super) fn join_words(text: &str) -> String {
    let mut joined = String::with_capacity(text.len());
    for word in text.split_whitespace() {
        let glued = word.chars().all(|c| GLUED.contains(&c));
        if joined.is_empty() && glued {
            continue;
        }
        if !joined.is_empty() && !glued {
            joined.push(' ');
        }
        joined.push_str(word);
    }
    joined
}

#[cfg(test)]
#[path = "display_plan_asides_tests.rs"]
mod tests;
