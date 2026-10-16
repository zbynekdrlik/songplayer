//! #280: the bare interjections the wall and the Presenter leave out.
//!
//! The base-tier transcript and Genius keep every sung `oh` / `yeah` / `hey`
//! as words, and the translator renders them literally (`Och`, `Áno`,
//! `Jeah`). The owner saw "One more time we say" + "Oh." shown as "Ešte raz
//! hovoríme Och." (16.10.2026). On SNV 1353 EN lines (4.1 %) of 349 songs
//! were only interjections (design 6223125533).
//!
//! The display plan reads each source line through [`shown`], after its
//! parenthesised asides went:
//!
//! - **The English line is the judge.** A line whose EN words are ALL
//!   interjections ([`EN`]) leaves the plan in both languages; the previous
//!   display line holds through its time by the plan's hold rules, and the
//!   line before it takes its sentence end ([`hand_back`]): "One more time
//!   we say" + "Oh." stays a sentence of its own.
//! - **Edges.** A run of interjections at the start of a line goes when its
//!   last word carries a mark (`Yeah, let's dance.` → `Let's dance.`), and
//!   one at the end when the word before it does (`like you. Oh,` →
//!   `like you.`). At least [`MIN_KEPT`] words must stay, else the line
//!   stays whole (`Oh, God.`). So `Say whoa oh.` and `Oh Lord, you're
//!   beautiful` stay whole: no mark separates their interjections.
//! - **A capital goes on.** A left-out line that began a sentence with a
//!   capital hands it to the next line kept ([`capitals`],
//!   [`with_capitals`]): `Oh,` + `what a God.` shows `What a God.`.
//! - **A lone mark is glued** to the word before it ([`join_words`]), so
//!   `Oh , yeah, I'm free` reads as `Oh, yeah, I'm free`.
//! - **Slovak follows an English cut, never alone:** at most as many words at
//!   that edge, each from [`SK`], under the same rule, down to one word
//!   ([`MIN_KEPT_SK`]). A Slovak `Áno` for an English "Yes" stays.
//!
//! Sung lyrics only: a dub line's English is not its translation (see
//! `display_plan.rs::shown_lines`).
//!
//! Display-only: the stored JSON, the karaoke view and the pipeline are
//! untouched.

use sp_core::lyrics::LyricsLine;

use super::asides::{GLUED, join_words};
use super::{SENTENCE_ENDS, final_mark, final_mark_at};

/// The English interjections, compared lower-case with the punctuation
/// around the word stripped (census on SNV, design 6224751018). `yes` is not
/// one: a sung "yes" is content.
const EN: [&str; 21] = [
    "oh", "ooh", "oooh", "oh-oh", "whoa", "woah", "woo", "yeah", "yea", "hey", "ah", "aah", "uh",
    "mm", "mmm", "hmm", "na", "la", "ay", "ayy", "eh",
];

/// The Slovak words that go where the English line lost an interjection at
/// that edge (the translator's `Och`, `Ó`, `Jeah`, and the `Áno` it made of
/// a sung "yeah").
const SK: [&str; 17] = [
    "och", "ó", "o", "oh", "jé", "jej", "hej", "áno", "jeah", "yeah", "ach", "ah", "uh", "mm",
    "hm", "wow", "ou",
];

/// The fewest words an edge cut may leave: `Oh, God.` stays whole.
const MIN_KEPT: usize = 2;

/// The fewest words the Slovak follow may leave (review round 2): the
/// English cut already showed that edge to be separate, and the wall shows
/// the Slovak, so `Ó, slobodní` under `Oh, we are free` → `We are free`
/// becomes `Slobodní`.
const MIN_KEPT_SK: usize = 1;

/// `line` as the wall shows it: `None` when its English is only
/// interjections, else with the edge runs its punctuation separates gone
/// (see the module docs). Times and word timings are unchanged.
pub(super) fn shown(line: &LyricsLine) -> Option<LyricsLine> {
    let joined = join_words(&line.en);
    let words: Vec<&str> = joined.split_whitespace().collect();
    if !words.is_empty() && words.iter().all(|word| is_in(&EN, word)) {
        return None;
    }
    let cut = edge_cut(&words, &EN, (words.len(), words.len()), MIN_KEPT);
    Some(LyricsLine {
        en: trimmed(&line.en, &words, cut),
        sk: line.sk.as_deref().map(|sk| {
            let joined = join_words(sk);
            let words: Vec<&str> = joined.split_whitespace().collect();
            trimmed(sk, &words, edge_cut(&words, &SK, cut, MIN_KEPT_SK))
        }),
        ..line.clone()
    })
}

/// A line left out hands its sentence end back to `before`, the last line
/// kept (review round 1): its English and its Slovak each end in the
/// left-out text's final `. ! ? …`, like the last word an end run left
/// ([`ended`]). Without it "One more time we say" + "Oh." ran into the next
/// sentence and lit it seconds early.
pub(super) fn hand_back(left: &LyricsLine, before: Option<&mut LyricsLine>) {
    let Some(before) = before else {
        return;
    };
    before.en = ended(&before.en, &left.en);
    if let Some(left_sk) = &left.sk {
        before.sk = before.sk.as_deref().map(|sk| ended(sk, left_sk));
    }
}

/// Whether `line`'s English and its Slovak begin with a capital: what a
/// left-out line that began a sentence hands to the next line kept
/// (follow-up 1: What A God's `Oh,` left `what a God, what a God.` in
/// lower case).
pub(super) fn capitals(line: &LyricsLine) -> (bool, bool) {
    (
        is_capitalised(&line.en),
        line.sk.as_deref().is_some_and(is_capitalised),
    )
}

/// `line` with the first letter of its English and of its Slovak a capital
/// where `capitals` (EN, SK) says so.
pub(super) fn with_capitals(mut line: LyricsLine, (en, sk): (bool, bool)) -> LyricsLine {
    if en {
        line.en = capitalised(&line.en);
    }
    if sk {
        line.sk = line.sk.as_deref().map(capitalised);
    }
    line
}

/// Whether `word`, lower-case with the punctuation around it stripped, is
/// one of `set`.
fn is_in(set: &[&str], word: &str) -> bool {
    let bare = word
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase();
    set.contains(&bare.as_str())
}

/// The final mark of `word` (closing quotes and brackets read past) when it
/// is one of `marks`.
fn mark_in(word: &str, marks: &[char]) -> Option<char> {
    final_mark(word).filter(|c| marks.contains(c))
}

/// How many words go at the start and at the end of `words`: the longest
/// run of `set` words at each edge, at most `most` words, whose last word
/// carries a mark ([`GLUED`]) at the start, or whose preceding word does at
/// the end. Nothing goes when fewer than `fewest` words would stay.
fn edge_cut(words: &[&str], set: &[&str], most: (usize, usize), fewest: usize) -> (usize, usize) {
    let in_set = |word: &&&str| is_in(set, word);
    let marked = |word: &str| mark_in(word, &GLUED).is_some();
    let start_run = words.iter().take_while(in_set).count().min(most.0);
    // The first word never belongs to the end run: it is the one before it.
    let end_run = words.iter().skip(1).rev().take_while(in_set).count();
    let k = (1..=start_run).rev().find(|&k| marked(words[k - 1]));
    let m = (1..=end_run.min(most.1))
        .rev()
        .find(|&m| marked(words[words.len() - m - 1]));
    let (k, m) = (k.unwrap_or(0), m.unwrap_or(0));
    if k + m + fewest <= words.len() {
        (k, m)
    } else {
        (0, 0)
    }
}

/// `text` (split into `words`) without its first `cut.0` and last `cut.1`
/// words, joined with one space; `text` as it is when nothing goes. The
/// first word left takes a capital when the line's first word had one, and
/// the last takes the line's final sentence end (`. ! ? …`) unless it ends a
/// sentence itself: `free indeed, yeah.` → `free indeed.`, `like you. Oh,` →
/// `like you.`.
fn trimmed(text: &str, words: &[&str], (k, m): (usize, usize)) -> String {
    if (k, m) == (0, 0) {
        return text.to_string();
    }
    let mut kept: Vec<String> = words[k..words.len() - m]
        .iter()
        .map(|word| word.to_string())
        .collect();
    if is_capitalised(words[0]) {
        kept[0] = capitalised(&kept[0]);
    }
    if let (Some(last), Some(line_end)) = (kept.last_mut(), words.last()) {
        *last = ended(last, line_end);
    }
    kept.join(" ")
}

/// `text` ending in the sentence end (`. ! ? …`) `from` ends in, unless
/// `text` is empty or ends a sentence itself: a sentence end survives the
/// words that carried it. Its final mark, when it has one of [`GLUED`], is
/// replaced (`free indeed,` → `free indeed.`), else the end follows it.
fn ended(text: &str, from: &str) -> String {
    let kept = text.trim_end();
    match mark_in(from.trim_end(), &SENTENCE_ENDS) {
        Some(end) if !kept.is_empty() && mark_in(kept, &SENTENCE_ENDS).is_none() => {
            match final_mark_at(kept) {
                Some(found) if GLUED.contains(&found.1) => replaced(kept, found, end),
                _ => format!("{kept}{end}"),
            }
        }
        _ => text.to_string(),
    }
}

/// Whether `word` (or a whole text) begins with a capital: its first letter
/// or digit, past any quote or mark, is an upper-case letter.
fn is_capitalised(word: &str) -> bool {
    word.chars()
        .find(|c| c.is_alphanumeric())
        .is_some_and(char::is_uppercase)
}

/// `word` (or a whole text) with its first letter or digit raised, past any
/// quote or mark: a digit stays as it is, so `3rd` never becomes `3Rd`
/// (review round 4).
fn capitalised(word: &str) -> String {
    match word.char_indices().find(|(_, c)| c.is_alphanumeric()) {
        Some((at, c)) => replaced(word, (at, c), c.to_uppercase()),
        None => word.to_string(),
    }
}

/// `word` with the char `c` at byte `at` replaced by `with`.
fn replaced(word: &str, (at, c): (usize, char), with: impl std::fmt::Display) -> String {
    format!("{}{with}{}", &word[..at], &word[at + c.len_utf8()..])
}

#[cfg(test)]
#[path = "display_plan_interjections_tests.rs"]
mod tests;
