//! Pure, WASM-safe auto-follow rules for the shared lyrics/subtitle panel
//! (#184 round F).
//!
//! The sp-ui `LyricsView` (Prehľad / Naživo / Dabing / the Lyrics details view)
//! scrolls ONLY its own scroller so the active line sits in the middle of the
//! panel, and a manual wheel / touch / drag on the panel pauses that follow for
//! [`FOLLOW_PAUSE_MS`]. The geometry and the pause window live here — covered by
//! the workspace `Test` job and the diff-scoped mutation gate — because sp-ui has
//! no unit-test job (`.claude/rules/sp-ui-frontend.md`).

/// A manual scroll gesture on the panel (wheel / touchstart / pointerdown)
/// suspends auto-follow for this long, so the operator can look around the
/// transcript; follow resumes on its own afterwards.
pub const FOLLOW_PAUSE_MS: f64 = 5000.0;

/// The scroller `scrollTop` that puts a line in the vertical middle of the
/// panel, clamped to the scroller's valid range `0..=max(0, content_h - view_h)`.
///
/// - `line_top` — the line's top edge in the scroller's CONTENT coordinates
///   (0 = the top of the scrollable content, independent of the current scroll).
/// - `line_h` — the line's rendered height.
/// - `view_h` — the scroller's visible height (`clientHeight`).
/// - `content_h` — the scroller's full content height (`scrollHeight`).
///
/// Near the top the result clamps to 0 (the first lines cannot be centred), near
/// the bottom to the last scrollable position, and content shorter than the view
/// never scrolls.
pub fn centered_scroll_top(line_top: f64, line_h: f64, view_h: f64, content_h: f64) -> f64 {
    let _ = (line_top, line_h, view_h, content_h);
    todo!("#184 round F RED")
}

/// Whether auto-follow is still suspended at `now_ms` by a manual scroll
/// gesture recorded at `paused_at_ms` (same monotonic clock). `None` = never
/// paused. The window is half-open: paused while `now − paused_at <
/// FOLLOW_PAUSE_MS`, following again from exactly `FOLLOW_PAUSE_MS` on.
pub fn follow_paused(now_ms: f64, paused_at_ms: Option<f64>) -> bool {
    let _ = (now_ms, paused_at_ms);
    todo!("#184 round F RED")
}

/// Whether the scroller must move from `current_top` to reach `target_top` —
/// a sub-pixel difference (browsers round `scrollTop`) is not worth a smooth
/// scroll, so it is skipped.
pub fn needs_scroll(current_top: f64, target_top: f64) -> bool {
    let _ = (current_top, target_top);
    todo!("#184 round F RED")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- centered_scroll_top: centre the line, clamp to the scroll range ----

    #[test]
    fn centered_scroll_top_centres_a_middle_line() {
        // line centre = 1000 + 40/2 = 1020; minus half the 260 px view = 890.
        assert_eq!(centered_scroll_top(1000.0, 40.0, 260.0, 3000.0), 890.0);
    }

    #[test]
    fn centered_scroll_top_uses_the_line_height_and_the_view_height() {
        // Different line + view heights move the result: 500 + 60/2 − 200/2 = 430.
        assert_eq!(centered_scroll_top(500.0, 60.0, 200.0, 3000.0), 430.0);
    }

    #[test]
    fn centered_scroll_top_clamps_the_first_lines_to_zero() {
        // 50 + 20 − 130 = −60 → the panel cannot scroll above its top.
        assert_eq!(centered_scroll_top(50.0, 40.0, 260.0, 3000.0), 0.0);
    }

    #[test]
    fn centered_scroll_top_clamps_the_last_lines_to_the_bottom() {
        // 2950 + 20 − 130 = 2840, but the last scrollable position is
        // 3000 − 260 = 2740.
        assert_eq!(centered_scroll_top(2950.0, 40.0, 260.0, 3000.0), 2740.0);
    }

    #[test]
    fn centered_scroll_top_reaches_the_bottom_exactly_at_the_limit() {
        // 2850 + 20 − 130 = 2740 == max → the bottom, unclamped.
        assert_eq!(centered_scroll_top(2850.0, 40.0, 260.0, 3000.0), 2740.0);
    }

    #[test]
    fn centered_scroll_top_never_scrolls_content_shorter_than_the_view() {
        // 180 + 10 − 130 = 60 would be positive, but 200 px of content in a
        // 260 px view has no scroll range at all.
        assert_eq!(centered_scroll_top(180.0, 20.0, 260.0, 200.0), 0.0);
    }

    // ---- follow_paused: a manual gesture suspends follow for 5 s ----

    #[test]
    fn follow_is_not_paused_without_a_gesture() {
        assert!(!follow_paused(123_456.0, None));
    }

    #[test]
    fn follow_is_paused_right_after_a_gesture() {
        assert!(follow_paused(10_000.0, Some(10_000.0)));
        assert!(follow_paused(14_999.0, Some(10_000.0)));
    }

    #[test]
    fn follow_resumes_exactly_at_the_pause_window() {
        assert!(!follow_paused(15_000.0, Some(10_000.0)));
        assert!(!follow_paused(20_000.0, Some(10_000.0)));
    }

    #[test]
    fn follow_pause_window_is_five_seconds() {
        assert_eq!(FOLLOW_PAUSE_MS, 5000.0);
    }

    // ---- needs_scroll: skip sub-pixel moves ----

    #[test]
    fn needs_scroll_skips_a_sub_pixel_difference() {
        assert!(!needs_scroll(100.0, 100.0));
        assert!(!needs_scroll(100.0, 100.4));
        assert!(!needs_scroll(100.4, 100.0));
    }

    #[test]
    fn needs_scroll_moves_one_pixel_or_more_in_either_direction() {
        assert!(needs_scroll(100.0, 101.0));
        assert!(needs_scroll(101.0, 100.0));
        assert!(needs_scroll(0.0, 890.0));
    }
}
