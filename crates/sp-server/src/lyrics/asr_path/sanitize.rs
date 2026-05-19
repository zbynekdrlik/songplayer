//! Line-level sanitizer shared by `resolver` and `fallback`.
//!
//! Enforces three invariants on a `Vec<LyricsLine>`:
//! - monotonic `start_ms` (each line's start >= previous line's end)
//! - no overlap (line N+1 start clamped up to line N end if necessary)
//! - minimum 200ms duration (very short lines get clamped to start+200)
//!
//! Both `fallback` and `resolver` use the same invariants — this module keeps
//! the two callers in sync with a single implementation and a single test suite.

use sp_core::lyrics::LyricsLine;

pub const MIN_LINE_DURATION_MS: u64 = 200;

pub fn sanitize_lines(mut lines: Vec<LyricsLine>) -> Vec<LyricsLine> {
    let mut floor: u64 = 0;
    for line in &mut lines {
        if line.start_ms < floor {
            line.start_ms = floor;
        }
        if line.end_ms < line.start_ms + MIN_LINE_DURATION_MS {
            line.end_ms = line.start_ms + MIN_LINE_DURATION_MS;
        }
        floor = line.end_ms;
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(start: u64, end: u64) -> LyricsLine {
        LyricsLine {
            start_ms: start,
            end_ms: end,
            en: "x".into(),
            sk: None,
            words: None,
        }
    }

    #[test]
    fn clamps_start_below_floor_up_to_floor() {
        // Kills fallback L63 / resolver L67: `<` → `==` (only clamps when
        // equal, misses smaller), `<` → `>` (only clamps when larger).
        // Second line has start=200 which is below floor=500 after first line.
        // Must be clamped to 500.
        let input = vec![line(0, 500), line(200, 800)];
        let out = sanitize_lines(input);
        assert_eq!(out[0].start_ms, 0);
        assert_eq!(out[0].end_ms, 500);
        // Second line: start was 200 < floor (500) → must be clamped to 500.
        assert_eq!(out[1].start_ms, 500);
        // end=800 >= start(500)+MIN(200)=700, so end unchanged.
        assert_eq!(out[1].end_ms, 800);
    }

    #[test]
    fn does_not_clamp_start_above_floor() {
        // Kills `<` → `>`: mutation would clamp start=600 (above floor=500)
        // because 600 > 500 is true. Original `<` correctly leaves it alone.
        let input = vec![line(0, 500), line(600, 900)];
        let out = sanitize_lines(input);
        assert_eq!(
            out[1].start_ms, 600,
            "start above floor must not be clamped"
        );
    }

    #[test]
    fn clamps_end_below_minimum_duration() {
        // Kills fallback L66 / resolver L70: `<` → `==`, and
        // kills fallback L66 / resolver L71: `+` → `*`.
        // Input: start=1000, end=1050 (50ms — under MIN=200).
        // Original: 1050 < 1000+200=1200 → clamp end to 1200.
        // Mutation `+→*`: 1050 < 1000*200=200000 → clamp to 200000 (way too big).
        // Mutation `<→==`: only clamps if 1050 == 1200 (false) → no clamp.
        let input = vec![line(1000, 1050)];
        let out = sanitize_lines(input);
        assert_eq!(out[0].start_ms, 1000);
        assert_eq!(out[0].end_ms, 1200);
    }

    #[test]
    fn end_far_above_minimum_duration_is_unchanged() {
        // Additional kill for `+→*` with a small start_ms:
        // start=10, end=500, MIN=200.
        // Original:  10+200=210, 500 >= 210 → no clamp, end stays 500.
        // Mutation `*`: 10*200=2000, 500 < 2000 → clamp to 2000. WRONG.
        let input = vec![line(10, 500)];
        let out = sanitize_lines(input);
        assert_eq!(
            out[0].end_ms, 500,
            "end far above minimum must not be clamped"
        );
    }

    #[test]
    fn enforces_no_overlap_between_lines() {
        // Adversarial input: line 2's start is between line 1's start and end.
        let input = vec![line(0, 1000), line(500, 1500)];
        let out = sanitize_lines(input);
        assert_eq!(
            out[1].start_ms, 1000,
            "start must be clamped up to prev end"
        );
    }

    #[test]
    fn start_exactly_equal_to_floor_is_not_clamped() {
        // Equivalent mutation `<` → `<=` would clamp start==floor unnecessarily.
        // Correct behavior: start==floor is already valid, no change needed.
        let input = vec![line(0, 500), line(500, 900)];
        let out = sanitize_lines(input);
        assert_eq!(
            out[1].start_ms, 500,
            "start exactly at floor is valid, no clamp"
        );
    }

    #[test]
    fn end_exactly_at_minimum_duration_boundary_is_not_clamped() {
        // Equivalent mutation `<` → `<=` would clamp end==start+MIN.
        // Correct: end==start+MIN means duration==MIN, which is fine.
        let input = vec![line(100, 300)]; // 300 == 100 + 200, exactly MIN
        let out = sanitize_lines(input);
        assert_eq!(
            out[0].end_ms, 300,
            "end exactly at minimum is valid, no clamp"
        );
    }

    #[test]
    fn empty_input_yields_empty_output() {
        assert!(sanitize_lines(vec![]).is_empty());
    }

    #[test]
    fn single_line_passes_through_when_well_formed() {
        let input = vec![line(100, 500)];
        let out = sanitize_lines(input);
        assert_eq!(out[0].start_ms, 100);
        assert_eq!(out[0].end_ms, 500);
    }
}
