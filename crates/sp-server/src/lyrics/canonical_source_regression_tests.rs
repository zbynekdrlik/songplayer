//! Canonical-source regression tests.
//!
//! Per `feedback_canonical_source_regression_ci.md`: pin the expected
//! `best_authoritative_candidate` source for known wall-verified anchor
//! songs. A silent provider regression (e.g. accidentally bumping genius
//! priority) breaks this test before the wall ever sees the wrong output.
//!
//! Each fixture below mirrors the production gather.rs candidate-text shape
//! for one anchor song. When a new song is wall-verified during the
//! song-by-song iteration loop (per `feedback_song_by_song_iteration.md`),
//! add a fixture here.

#![allow(unused_imports)]

use crate::lyrics::claude_merge::best_authoritative_candidate;
use crate::lyrics::tier1::CandidateText;

fn text_cand(source: &str, line_count: usize) -> CandidateText {
    CandidateText {
        source: source.into(),
        lines: vec!["x".into(); line_count],
        line_timings: None,
        has_timing: false,
    }
}

/// id=132 "Holy Forever" / Chris Tomlin — wall-verified anchor 2026-05-05.
/// Production candidate set (observed): description present (clean lyrics
/// in YT description), no genius hit (artist/song mismatch in Genius DB).
/// Expected best: description.
#[test]
fn id_132_holy_forever_picks_description() {
    let candidates = vec![text_cand("description", 24)];
    let best = best_authoritative_candidate(&candidates).unwrap();
    assert_eq!(
        best.source, "description",
        "id=132 'Holy Forever' must pick description; observed wall-verified anchor"
    );
}

/// id=21 "Good Shepherd" / Chroma Worship — wall-verified post-fix anchor.
/// Production candidate set (observed in 2026-05-07 reprocess log):
/// description (26 lines, clean) + genius (70 lines, longer/looser).
/// Pre-fix: genius won (priority 2 > description 0). Post-fix: description
/// wins (priority 3 > genius 1). This fixture is the regression detector.
#[test]
fn id_21_good_shepherd_picks_description_over_genius() {
    let candidates = vec![text_cand("description", 26), text_cand("genius", 70)];
    let best = best_authoritative_candidate(&candidates).unwrap();
    assert_eq!(
        best.source, "description",
        "id=21 'Good Shepherd' must pick description over genius post-2026-05-07-fix; \
         a silent regression here breaks the wall on every genius-hit song"
    );
}

/// Future-anchor template — copy this when a new song is wall-verified.
/// Replace the fixture and the expected source with the observed values
/// from the song's `iDuKrk2lI5U_lyrics.json` (or equivalent) file.
#[test]
fn anchor_template_placeholder() {
    // Placeholder so the test module isn't empty if the two anchors above
    // get edited. Real anchors live in their own #[test] above.
    let candidates: Vec<CandidateText> = vec![];
    assert!(best_authoritative_candidate(&candidates).is_none());
}
