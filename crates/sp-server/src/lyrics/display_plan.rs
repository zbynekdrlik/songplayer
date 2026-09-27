//! The LED-wall / Presenter display plan of one lyrics track (#217).
//!
//! The stored lyrics lines are sung-timing units. The base tier
//! (`gemini-3-5-transcribe`) in particular splits verses into short,
//! mid-sentence fragments. Showing them one by one at their sung times made the
//! wall blink: a fragment flashed for 0.2–1.5 s, and the wall went dark in every
//! gap. On "What A God" (`6KuPjo1diLg`) that was 310 s dark out of 633 s.
//!
//! [`build_plan`] turns the lines into what the wall shows, once per loaded
//! track, and never touches the stored JSON or the lyrics pipeline:
//!
//! 1. **Merge.** A fragment is a line sung for less than [`FRAGMENT_MAX_MS`],
//!    one with at most [`FRAGMENT_MAX_WORDS`] words, or one whose next line
//!    starts lowercase (a continuation). A fragment merges with its following
//!    line when the gap is at most [`MERGE_MAX_GAP_MS`] and the joined English
//!    text fits in [`MERGE_MAX_CHARS`] chars. If it cannot merge forward, it
//!    merges backward under the same limits. The Slovak text is joined the same
//!    way. Merging repeats until no rule applies.
//! 2. **Lead.** A line shows at `max(start - LEAD_MS, prev.show +
//!    MIN_VISIBLE_MS)`, but never after it is sung (`<= start`): when the
//!    previous lines leave no room, it shows exactly when it is sung. The
//!    lead can replace the previous line while that line is still sung. In a
//!    fast run it can even replace it before its sung start (see the
//!    design question on #217).
//! 3. **Hold.** A line stays on the wall until the next line shows. Before an
//!    instrumental break (a gap over [`LONG_GAP_MS`]) and after the last line,
//!    it leaves [`HOLD_TAIL_MS`] after its sung end instead.
//! 4. **Minimum visibility.** A line the schedule leaves on the wall for less
//!    than [`MIN_VISIBLE_MS`] is merged again under the same limits. If no merge
//!    fits, it is shown exactly.
//!
//! The dashboard karaoke view (`LyricsState::update`) keeps the raw track and
//! its word timing. Only the wall and the Presenter read this plan. The
//! constants are documented in `.claude/rules/lyrics-display.md`.

use std::ops::Range;

use sp_core::lyrics::LyricsLine;

/// How long before it is sung a line appears on the wall, so the room can
/// pre-read it.
pub const LEAD_MS: u64 = 1_500;

/// The shortest time a line stays on the wall (no blinking). A line gets less
/// only when the next line is sung sooner than that after it.
pub const MIN_VISIBLE_MS: u64 = 1_200;

/// A gap between two sung lines longer than this is an instrumental break, and
/// the wall goes blank for it. A shorter gap keeps the line on the wall.
pub const LONG_GAP_MS: u64 = 8_000;

/// Before an instrumental break, and after the last line, a line stays this
/// long after its sung end.
pub const HOLD_TAIL_MS: u64 = 3_000;

/// A line sung for less than this is a fragment.
pub const FRAGMENT_MAX_MS: u64 = 1_500;

/// A line with at most this many words is a fragment.
pub const FRAGMENT_MAX_WORDS: usize = 3;

/// A fragment merges with a neighbour only across a gap of at most this.
pub const MERGE_MAX_GAP_MS: u64 = 700;

/// A merge must keep the joined English text within this many chars (what fits
/// the wall).
pub const MERGE_MAX_CHARS: usize = 64;

/// One line as the LED wall and the Presenter show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayLine {
    /// English text: the merged source lines' texts joined with one space.
    pub en: String,
    /// Slovak text, joined the same way; `None` when no source line has one.
    pub sk: Option<String>,
    /// Track position (ms) at which the line appears on the wall.
    pub show_ms: u64,
    /// Track position (ms) at which it leaves the wall. This is the next
    /// line's `show_ms`, or `sung end + HOLD_TAIL_MS` before a long break and
    /// after the last line.
    pub hide_ms: u64,
    /// The indices of the source lines (in `LyricsTrack::lines`) merged into
    /// this line.
    pub src_range: Range<usize>,
}

/// The display plan of one track, built once when the track loads.
#[derive(Debug, Clone)]
pub struct DisplayPlan {
    lines: Vec<DisplayLine>,
}

impl DisplayPlan {
    /// Build the plan for a track's lines (see [`build_plan`]).
    pub fn build(lines: &[LyricsLine]) -> Self {
        Self {
            lines: build_plan(lines),
        }
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

/// Turn a track's sung lines into its display lines: merge, then schedule (see
/// the module docs).
pub fn build_plan(lines: &[LyricsLine]) -> Vec<DisplayLine> {
    let groups = merge_groups(lines);
    let spans = schedule(&groups);
    groups
        .into_iter()
        .zip(spans)
        .map(|(group, (show_ms, hide_ms))| DisplayLine {
            en: group.en,
            sk: group.sk,
            show_ms,
            hide_ms,
            src_range: group.src,
        })
        .collect()
}

/// A run of consecutive source lines that the wall shows as one line.
#[derive(Debug, Clone)]
struct Group {
    en: String,
    sk: Option<String>,
    start_ms: u64,
    end_ms: u64,
    src: Range<usize>,
}

impl Group {
    fn of(index: usize, line: &LyricsLine) -> Self {
        Self {
            en: line.en.clone(),
            sk: line.sk.clone(),
            start_ms: line.start_ms,
            end_ms: line.end_ms,
            src: index..index + 1,
        }
    }

    /// Append the group that follows this one.
    fn absorb(&mut self, next: Group) {
        self.en = join_text(&self.en, &next.en);
        self.sk = join_sk(self.sk.take(), next.sk);
        self.end_ms = self.end_ms.max(next.end_ms);
        self.src.end = next.src.end;
    }
}

/// Merge the lines until no merge applies. Each merge removes one group, so
/// the loop ends after at most `lines.len() - 1` merges.
fn merge_groups(lines: &[LyricsLine]) -> Vec<Group> {
    let mut groups: Vec<Group> = lines
        .iter()
        .enumerate()
        .map(|(index, line)| Group::of(index, line))
        .collect();
    while let Some(i) = next_merge(&groups) {
        let next = groups.remove(i + 1);
        groups[i].absorb(next);
    }
    groups
}

/// The next merge to apply, as the index `i` that absorbs group `i + 1`. Text
/// fragments come first. Once none is left, a line the schedule shows for less
/// than [`MIN_VISIBLE_MS`] is tried.
fn next_merge(groups: &[Group]) -> Option<usize> {
    fragment_merge(groups).or_else(|| short_merge(groups, &schedule(groups)))
}

/// The first text fragment that fits a neighbour.
fn fragment_merge(groups: &[Group]) -> Option<usize> {
    groups
        .iter()
        .enumerate()
        .filter(|&(i, group)| is_fragment(group, groups.get(i + 1)))
        .find_map(|(i, _)| merge_target(groups, i))
}

/// The first line on the wall for less than [`MIN_VISIBLE_MS`] that fits a
/// neighbour.
fn short_merge(groups: &[Group], spans: &[(u64, u64)]) -> Option<usize> {
    spans
        .iter()
        .enumerate()
        .filter(|&(_, &(show_ms, hide_ms))| hide_ms.saturating_sub(show_ms) < MIN_VISIBLE_MS)
        .find_map(|(i, _)| merge_target(groups, i))
}

/// Where group `i` merges: forward into its following group when they fit
/// (returns `i`), else backward into the previous one (returns `i - 1`).
fn merge_target(groups: &[Group], i: usize) -> Option<usize> {
    let group = &groups[i];
    if groups.get(i + 1).is_some_and(|next| fits(group, next)) {
        return Some(i);
    }
    let prev = i.checked_sub(1)?;
    fits(&groups[prev], group).then_some(prev)
}

/// A fragment: sung for less than [`FRAGMENT_MAX_MS`], at most
/// [`FRAGMENT_MAX_WORDS`] words, or followed by a lowercase continuation.
fn is_fragment(group: &Group, next: Option<&Group>) -> bool {
    group.end_ms.saturating_sub(group.start_ms) < FRAGMENT_MAX_MS
        || group.en.split_whitespace().count() <= FRAGMENT_MAX_WORDS
        || next.is_some_and(|next| starts_lowercase(&next.en))
}

/// Whether `a` and the group `b` that follows it may merge: a gap of at most
/// [`MERGE_MAX_GAP_MS`] and a joined English text of at most
/// [`MERGE_MAX_CHARS`] chars. Overlapping lines have no gap.
fn fits(a: &Group, b: &Group) -> bool {
    b.start_ms.saturating_sub(a.end_ms) <= MERGE_MAX_GAP_MS
        && join_text(&a.en, &b.en).chars().count() <= MERGE_MAX_CHARS
}

/// Whether the text continues a sentence: its first letter or digit is a
/// lowercase letter. Leading punctuation (`'cause`, `…and`) is skipped.
fn starts_lowercase(text: &str) -> bool {
    text.chars()
        .find(|c| c.is_alphanumeric())
        .is_some_and(char::is_lowercase)
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

/// The `(show_ms, hide_ms)` of every group (lead, hold, long-break tail; see
/// the module docs).
fn schedule(groups: &[Group]) -> Vec<(u64, u64)> {
    let mut shows: Vec<u64> = Vec::with_capacity(groups.len());
    for group in groups {
        let lead = group.start_ms.saturating_sub(LEAD_MS);
        let show = match shows.last() {
            Some(&prev) => lead.max(prev + MIN_VISIBLE_MS).min(group.start_ms),
            None => lead,
        };
        shows.push(show);
    }
    groups
        .iter()
        .enumerate()
        .map(|(i, group)| {
            let hide = match groups.get(i + 1) {
                Some(next) if next.start_ms.saturating_sub(group.end_ms) <= LONG_GAP_MS => {
                    shows[i + 1]
                }
                _ => group.end_ms + HOLD_TAIL_MS,
            };
            (shows[i], hide)
        })
        .collect()
}

#[cfg(test)]
#[path = "display_plan_tests.rs"]
mod tests;
