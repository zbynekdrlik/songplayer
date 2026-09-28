//! The LED-wall / Presenter display plan of one lyrics track (#217).
//!
//! The stored lyrics lines are sung-timing units. The base tier
//! (`gemini-3-5-transcribe`) in particular splits verses into short
//! punctuated phrases: "Now his will be", "done.", "Lift up your", …
//! Shown one by one at their sung times, the wall blinked and went dark in
//! every gap. Merged by length, they ran across sentence ends ("done. Lift up
//! your banners…"), so the wall switched to the next sentence while the
//! singers were still finishing the last one (owner, 28.9.2026).
//!
//! [`build_plan`] turns the lines into what the wall shows, once per loaded
//! track, and never touches the stored JSON or the lyrics pipeline
//! (design record 5867952012):
//!
//! 1. **Sentences.** Consecutive source lines form one display line until a
//!    line ends a sentence: its text ends in `. ! ? …`, optionally followed by
//!    closing quotes or brackets. A line's text is its English, or its Slovak
//!    when it has none (a dub subtitle line). The display line closes earlier
//!    when the next line would not fit it:
//!    - its text would get over [`MAX_CHARS`] chars, or the next line starts
//!      over [`GROUP_MAX_SPAN_MS`] after its first line: it closes after its
//!      last line ending in `, ; : —` (a soft end), else whole, and the rest
//!      is checked again;
//!    - the next line starts over [`LONG_GAP_MS`] after it is sung (an
//!      instrumental break): it closes whole.
//!
//!    A display line spans whole source lines. A line cannot be split without
//!    word timings, and those are never synthesized.
//! 2. **Show.** A `Song` line shows when its first line starts being sung. It
//!    leads by up to [`LEAD_MAX_MS`] only into a real pause, once the previous
//!    display line has been sung to its end [`SUSTAIN_MARGIN_MS`] ago:
//!    `show = min(max(start − LEAD_MAX_MS, prev_end + SUSTAIN_MARGIN_MS),
//!    start)`. It then waits until the previous line has been up for
//!    [`MIN_VISIBLE_MS`] (`show ≥ prev_show + MIN_VISIBLE_MS`), so a line
//!    sung sooner than that after the previous one shows a little late.
//!    A `Speech` line shows exactly when it is spoken.
//! 3. **Hold.** A line stays on the wall until the next line shows. Before an
//!    instrumental break (a gap over [`LONG_GAP_MS`]) and after the last line,
//!    it leaves [`HOLD_TAIL_MS`] after its sung end instead.
//!
//! The dashboard karaoke view (`LyricsState::update`) keeps the raw track and
//! its word timing. Only the wall and the Presenter read this plan. The
//! constants are documented in `.claude/rules/lyrics-display.md`.

use std::ops::Range;

use sp_core::lyrics::LyricsLine;

use crate::dabing::subtitles::SOURCE_LIVE_TRANSLATE;

/// The most a `Song` line appears on the wall before it is sung, so the room
/// can pre-read it. Only into a pause of [`SUSTAIN_MARGIN_MS`] after the
/// previous line; `Speech` has no lead.
pub const LEAD_MAX_MS: u64 = 800;

/// A `Song` line may appear early only this long after the previous display
/// line's last sung line ends. A transcript line ends before a held note does,
/// so the margin keeps the next sentence off the wall while the last one is
/// still being sung.
pub const SUSTAIN_MARGIN_MS: u64 = 1_500;

/// The shortest time a `Song` line stays on the wall (no blinking). The next
/// line waits for it, even past its own sung start.
pub const MIN_VISIBLE_MS: u64 = 1_200;

/// A gap between two sung lines longer than this is an instrumental break, and
/// the wall goes blank for it. A shorter gap keeps the line on the wall.
pub const LONG_GAP_MS: u64 = 8_000;

/// Before an instrumental break, and after the last line, a line stays this
/// long after its sung end.
pub const HOLD_TAIL_MS: u64 = 3_000;

/// The most chars one display line's text may have (what fits the wall). A
/// single source line over it is shown whole.
pub const MAX_CHARS: usize = 72;

/// The last source line of a display line starts at most this long after its
/// first one, so a long sentence never lights its end seconds early.
pub const GROUP_MAX_SPAN_MS: u64 = 6_500;

/// Marks that end a sentence.
const SENTENCE_ENDS: [char; 4] = ['.', '!', '?', '…'];

/// Marks where an over-long sentence may split (a soft end).
const SOFT_ENDS: [char; 4] = [',', ';', ':', '—'];

/// Closing quotes and brackets, skipped when reading a line's final mark.
const CLOSERS: [char; 7] = ['"', '\'', '”', '’', '»', ')', ']'];

/// How a track is shown on the wall, chosen when the track loads (#217).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayProfile {
    /// Sung lyrics: a line may appear up to [`LEAD_MAX_MS`] before it is
    /// sung, into a pause, and stays up at least [`MIN_VISIBLE_MS`].
    Song,
    /// Dub subtitles of speech (`gemini-live-translate`): each line appears
    /// exactly when it is spoken. Grouping and hold apply as for songs.
    Speech,
}

impl DisplayProfile {
    /// The profile of a track by its `source` label. A dub subtitle track
    /// ([`SOURCE_LIVE_TRANSLATE`], the marker the dub builder stamps and the
    /// translation worker already excludes by) is speech; everything else is
    /// a song.
    pub fn for_source(source: &str) -> Self {
        if source == SOURCE_LIVE_TRANSLATE {
            Self::Speech
        } else {
            Self::Song
        }
    }

    /// When a display line whose first line starts being sung at `start_ms`
    /// appears, given the previous display line's `(show_ms, sung end)` (see
    /// step 2 of the module docs).
    fn show_ms(self, start_ms: u64, prev: Option<(u64, u64)>) -> u64 {
        let early = start_ms.saturating_sub(LEAD_MAX_MS);
        match (self, prev) {
            (Self::Speech, _) => start_ms,
            (Self::Song, None) => early,
            (Self::Song, Some((prev_show, prev_end))) => early
                .max(prev_end + SUSTAIN_MARGIN_MS)
                .min(start_ms)
                .max(prev_show + MIN_VISIBLE_MS),
        }
    }
}

/// One line as the LED wall and the Presenter show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayLine {
    /// English text: the source lines' texts joined with one space.
    pub en: String,
    /// Slovak text, joined the same way; `None` when no source line has one.
    pub sk: Option<String>,
    /// Track position (ms) at which the line appears on the wall.
    pub show_ms: u64,
    /// Track position (ms) at which it leaves the wall. This is the next
    /// line's `show_ms`, or `sung end + HOLD_TAIL_MS` before a long break and
    /// after the last line.
    pub hide_ms: u64,
    /// The indices of the source lines (in `LyricsTrack::lines`) this line
    /// shows.
    pub src_range: Range<usize>,
}

/// The display plan of one track, built once when the track loads.
#[derive(Debug, Clone)]
pub struct DisplayPlan {
    lines: Vec<DisplayLine>,
    profile: DisplayProfile,
}

impl DisplayPlan {
    /// Build the plan for a track's lines under `profile` (see [`build_plan`]).
    pub fn build(lines: &[LyricsLine], profile: DisplayProfile) -> Self {
        Self {
            lines: build_plan(lines, profile),
            profile,
        }
    }

    /// The profile this plan was built under.
    pub fn profile(&self) -> DisplayProfile {
        self.profile
    }

    /// Every display line, in wall order.
    pub fn lines(&self) -> &[DisplayLine] {
        &self.lines
    }

    /// The line on the wall at track position `position_ms`
    /// (`show_ms <= position_ms < hide_ms`), with its index. `None` before
    /// the first line, in the blank stretch of an instrumental break, and after
    /// the last line leaves.
    pub fn at(&self, position_ms: u64) -> Option<(usize, &DisplayLine)> {
        self.lines
            .iter()
            .enumerate()
            .find(|(_, line)| line.show_ms <= position_ms && position_ms < line.hide_ms)
    }
}

/// Turn a track's sung lines into its display lines under `profile`: group
/// them into sentences, then schedule (see the module docs).
pub fn build_plan(lines: &[LyricsLine], profile: DisplayProfile) -> Vec<DisplayLine> {
    let groups = group_lines(lines);
    let spans = schedule(lines, &groups, profile);
    groups
        .into_iter()
        .zip(spans)
        .map(|(group, (show_ms, hide_ms))| {
            let members = &lines[group.clone()];
            DisplayLine {
                en: members
                    .iter()
                    .fold(String::new(), |en, line| join_text(&en, &line.en)),
                sk: members
                    .iter()
                    .fold(None, |sk, line| join_sk(sk, line.sk.clone())),
                show_ms,
                hide_ms,
                src_range: group,
            }
        })
        .collect()
}

/// The source lines of each display line, in order (step 1 of the module
/// docs).
fn group_lines(lines: &[LyricsLine]) -> Vec<Range<usize>> {
    let mut groups = Vec::new();
    // The first source line of the open display line, which runs up to the
    // line at hand.
    let mut open: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        if let Some(first) = open {
            open = close_to_fit(lines, first..i, &mut groups);
        }
        let first = *open.get_or_insert(i);
        if ends_sentence(line) {
            groups.push(first..i + 1);
            open = None;
        }
    }
    groups.extend(open.map(|first| first..lines.len()));
    groups
}

/// Close as much of the open display line `open` as the source line after it
/// (`lines[open.end]`) needs to join what stays open, pushing each closed part
/// to `groups`. Before an instrumental break the whole line closes. When the
/// next line would not [`fits`], the line closes after its last soft end (or
/// whole, with none). The rest has no soft end, so it either takes the next
/// line or closes whole too. Returns the first line of what stays open.
fn close_to_fit(
    lines: &[LyricsLine],
    open: Range<usize>,
    groups: &mut Vec<Range<usize>>,
) -> Option<usize> {
    let members = &lines[open.clone()];
    if lines[open.end].start_ms.saturating_sub(sung_end(members)) > LONG_GAP_MS {
        groups.push(open);
        return None;
    }
    if fits(&lines[open.start..=open.end]) {
        return Some(open.start);
    }
    let head = members
        .iter()
        .rposition(ends_softly)
        .map_or(members.len(), |k| k + 1);
    let rest = open.start + head..open.end;
    groups.push(open.start..rest.start);
    if rest.is_empty() {
        return None;
    }
    if fits(&lines[rest.start..=open.end]) {
        return Some(rest.start);
    }
    groups.push(rest);
    None
}

/// Whether consecutive source lines fit one display line: their joined text
/// is at most [`MAX_CHARS`] chars, and the last starts at most
/// [`GROUP_MAX_SPAN_MS`] after the first.
fn fits(members: &[LyricsLine]) -> bool {
    let (Some(first), Some(last)) = (members.first(), members.last()) else {
        return true;
    };
    let joined = members
        .iter()
        .fold(String::new(), |acc, line| join_text(&acc, text(line)));
    joined.chars().count() <= MAX_CHARS
        && last.start_ms.saturating_sub(first.start_ms) <= GROUP_MAX_SPAN_MS
}

/// The text a line is grouped by: its English, or its Slovak when it has
/// none (a dub subtitle line). Trimmed.
fn text(line: &LyricsLine) -> &str {
    match line.en.trim() {
        "" => line.sk.as_deref().unwrap_or_default().trim(),
        en => en,
    }
}

/// The last char of `text` that is not a closing quote or bracket.
fn final_mark(text: &str) -> Option<char> {
    text.chars().rev().find(|c| !CLOSERS.contains(c))
}

/// Whether the line ends a sentence (`. ! ? …`).
fn ends_sentence(line: &LyricsLine) -> bool {
    final_mark(text(line)).is_some_and(|c| SENTENCE_ENDS.contains(&c))
}

/// Whether the line ends in a soft end (`, ; : —`).
fn ends_softly(line: &LyricsLine) -> bool {
    final_mark(text(line)).is_some_and(|c| SOFT_ENDS.contains(&c))
}

/// When the last of `members` is sung to its end.
fn sung_end(members: &[LyricsLine]) -> u64 {
    members
        .iter()
        .map(|line| line.end_ms)
        .max()
        .unwrap_or_default()
}

/// Join two texts with one space. The texts are trimmed, and an empty side is
/// dropped.
fn join_text(a: &str, b: &str) -> String {
    match (a.trim(), b.trim()) {
        ("", b) => b.to_string(),
        (a, "") => a.to_string(),
        (a, b) => format!("{a} {b}"),
    }
}

/// Join two optional Slovak texts: both present are joined, one present is
/// kept, and none stays `None`.
fn join_sk(a: Option<String>, b: Option<String>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(join_text(&a, &b)),
        (a, b) => a.or(b),
    }
}

/// The `(show_ms, hide_ms)` of every display line (steps 2 and 3 of the
/// module docs).
fn schedule(
    lines: &[LyricsLine],
    groups: &[Range<usize>],
    profile: DisplayProfile,
) -> Vec<(u64, u64)> {
    let mut shows: Vec<u64> = Vec::with_capacity(groups.len());
    // The previous display line's (show, sung end).
    let mut prev: Option<(u64, u64)> = None;
    for group in groups {
        let show = profile.show_ms(lines[group.start].start_ms, prev);
        shows.push(show);
        prev = Some((show, sung_end(&lines[group.clone()])));
    }
    groups
        .iter()
        .enumerate()
        .map(|(i, group)| {
            let end = sung_end(&lines[group.clone()]);
            let hide = match groups.get(i + 1) {
                Some(next) if lines[next.start].start_ms.saturating_sub(end) <= LONG_GAP_MS => {
                    shows[i + 1]
                }
                _ => end + HOLD_TAIL_MS,
            };
            (shows[i], hide)
        })
        .collect()
}

#[cfg(test)]
#[path = "display_plan_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "display_plan_fixture_tests.rs"]
mod fixture_tests;
