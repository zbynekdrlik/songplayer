use super::*;

#[test]
fn default_max_chars_is_32() {
    assert_eq!(DEFAULT_MAX_CHARS, 32);
}

// ── find_split_index: center / distance arithmetic (line-length splitter) ──

#[test]
fn find_split_index_returns_none_for_short_text() {
    assert_eq!(find_split_index("Hello", 5), None);
    assert_eq!(find_split_index("Hello", 6), None);
    assert_eq!(find_split_index("Hello", 100), None);
}

#[test]
fn find_split_index_word_boundary_nearest_center() {
    let result = find_split_index("aaaa bbbb cccc dddd", 10);
    assert!(result.is_some());
    let idx = result.unwrap();
    assert!(idx > 0);
    assert!(idx < "aaaa bbbb cccc dddd".len());
    assert!("aaaa bbbb cccc dddd"[..idx].chars().count() <= 10);
}

#[test]
fn find_split_index_word_boundary_nearest_center_verified_value() {
    // "abcde fghij klmno" — spaces at byte 5 / 11, max_chars=9.
    // center=4, nearest space byte 5 → split at byte 6.
    let text = "abcde fghij klmno";
    let result = find_split_index(text, 9);
    assert_eq!(result, Some(6), "nearest-center split at byte 6");
}

#[test]
fn find_split_index_does_not_split_beyond_limit() {
    let text = "hello world and more text here";
    let result = find_split_index(text, 12);
    assert!(result.is_some());
    let idx = result.unwrap();
    assert!(
        text[..idx].chars().count() <= 12,
        "left half must be ≤ max_chars"
    );
}

#[test]
fn find_split_index_rightmost_word_boundary_fallback() {
    let text = "abc def ghij klmno";
    let result = find_split_index(text, 10);
    assert!(result.is_some());
    let idx = result.unwrap();
    assert!(text[..idx].chars().count() <= 10);
    assert!(idx > 0);
}

#[test]
fn find_split_index_split_point_is_after_space_not_at_space() {
    let text = "hello world how are you doing here";
    let result = find_split_index(text, 15);
    assert!(result.is_some());
    let idx = result.unwrap();
    let byte_at_idx = text.as_bytes().get(idx).copied().unwrap_or(0);
    assert_ne!(
        byte_at_idx, b' ',
        "split idx must point past the space, not at it"
    );
}

#[test]
fn find_split_index_center_division_not_modulo() {
    // "a bbbbb ccccc" max_chars=10 → nearest-center space at byte 7 → Some(8).
    let result = find_split_index("a bbbbb ccccc", 10);
    assert_eq!(
        result,
        Some(8),
        "nearest-center split must use division, not modulo"
    );
}

#[test]
fn find_split_index_center_division_not_multiplication() {
    // "aaaa bb ccc dddd" max_chars=8 → nearest-center space at byte 4 → Some(5).
    let result = find_split_index("aaaa bb ccc dddd", 8);
    assert_eq!(
        result,
        Some(5),
        "nearest-center split must use division, not multiplication"
    );
}

#[test]
fn find_split_index_nearest_center_uses_subtraction_for_distance() {
    // "aa bb ccccc ddd" max_chars=12 → nearest-center space at byte 5 → Some(6).
    let result = find_split_index("aa bb ccccc ddd", 12);
    assert_eq!(
        result,
        Some(6),
        "distance must be computed via subtraction, not addition or division"
    );
}

#[test]
fn find_split_index_equal_distance_picks_first_space() {
    // "aa bbbbb ccc ddd" max_chars=10 → equidistant spaces byte 2 / 8 → first → Some(3).
    let result = find_split_index("aa bbbbb ccc ddd", 10);
    assert_eq!(
        result,
        Some(3),
        "equal-distance spaces: first (leftmost) must be chosen, not last"
    );
}

#[test]
fn find_split_index_nearer_center_wins_over_farther() {
    // "a bbbbbb cccc" max_chars=10 → nearer space byte 8 → Some(9).
    let result = find_split_index("a bbbbbb cccc", 10);
    assert_eq!(
        result,
        Some(9),
        "nearer-to-center space must win over farther space"
    );
}

// ── split_lyrics_lines: the surviving line-level splitter (g35t base tier) ──

fn ll(start: u64, end: u64, en: &str) -> sp_core::lyrics::LyricsLine {
    sp_core::lyrics::LyricsLine {
        start_ms: start,
        end_ms: end,
        en: en.into(),
        sk: None,
        words: None,
    }
}

#[test]
fn lyrics_short_line_passes_through() {
    let out = split_lyrics_lines(vec![ll(0, 1000, "short line")], SplitConfig::default());
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].en, "short line");
}

#[test]
fn lyrics_long_line_splits_to_max_chars() {
    // "His name will bring complete breakthrough" = 41 chars > 32 → splits.
    let out = split_lyrics_lines(
        vec![ll(0, 4000, "His name will bring complete breakthrough")],
        SplitConfig::default(),
    );
    assert!(out.len() >= 2, "long line must split");
    assert!(
        out.iter().all(|l| l.en.chars().count() <= 32),
        "every output line ≤ 32 chars: {:?}",
        out.iter().map(|l| l.en.clone()).collect::<Vec<_>>()
    );
    assert_eq!(out[0].start_ms, 0);
    assert_eq!(out.last().unwrap().end_ms, 4000);
    assert!(
        out[0].end_ms > 0 && out[0].end_ms < 4000,
        "proportional mid split"
    );
    assert!(out.iter().all(|l| l.words.is_none()));
}

#[test]
fn lyrics_split_timing_is_proportional_not_uniform() {
    // Left half much longer than right → left gets proportionally more time
    // (NOT a 50/50 uniform split). Per feedback_no_even_distribution.
    let out = split_lyrics_lines(
        vec![ll(0, 1000, "averylongleadingword tiny")],
        SplitConfig { max_chars: 20 },
    );
    assert_eq!(out.len(), 2);
    assert!(
        out[0].end_ms > 700,
        "left half gets most of the time, got {}",
        out[0].end_ms
    );
}
