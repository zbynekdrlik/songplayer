//! Pure, WASM-safe seek helpers (#194 ROUND 1).
//!
//! Shared by the sp-ui `Player` (the seek bar + ±10 s buttons + position
//! readout) and the `POST /api/v1/playback/{id}/seek` route (server-side clamp).
//! No I/O, no platform code — covered by the workspace `Test` job and the
//! diff-scoped mutation gate, so the boundary behaviour is verified once and
//! reused everywhere instead of re-derived per call site.

/// New absolute position after nudging `current_ms` by `delta_ms`, clamped into
/// `0..=duration_ms`. `delta_ms` is signed so the same helper serves the
/// `−10 s` / `+10 s` buttons; passing `delta_ms = 0` clamps a raw position to
/// the song's duration (the server route's use).
pub fn seek_target_ms(current_ms: u64, delta_ms: i64, duration_ms: u64) -> u64 {
    let target = (current_ms as i64).saturating_add(delta_ms).max(0) as u64;
    target.min(duration_ms)
}

/// Progress fraction `pos_ms / dur_ms`, clamped to `0.0..=1.0`. Returns `0.0`
/// for a zero duration (no divide-by-zero, no `NaN`).
pub fn seek_fraction(pos_ms: u64, dur_ms: u64) -> f64 {
    if dur_ms == 0 {
        return 0.0;
    }
    (pos_ms as f64 / dur_ms as f64).clamp(0.0, 1.0)
}

/// The position a seek bar should DISPLAY (and bind to `prop:value`): while the
/// operator is dragging the thumb, the dragged value is authoritative so a live
/// position tick can't snap it back; otherwise the live position drives it.
/// #194 — pins the drag against the twice-a-second now-playing tick. Pure so the
/// gate's boundary behaviour is unit-tested (sp-ui has no unit-test job).
pub fn seek_display_ms(dragging: bool, dragged_ms: u64, live_ms: u64) -> u64 {
    if dragging { dragged_ms } else { live_ms }
}

/// Format a millisecond position as `M:SS` (seconds zero-padded to two digits).
/// Minutes are not wrapped at 60 — a 61-minute song reads `61:01`, matching the
/// operator's mental model of a single running counter.
pub fn format_position(ms: u64) -> String {
    let total_secs = ms / 1000;
    format!("{}:{:02}", total_secs / 60, total_secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- seek_target_ms: clamp current + delta into 0..=duration ----

    #[test]
    fn seek_target_forward_within_bounds() {
        assert_eq!(seek_target_ms(30_000, 10_000, 200_000), 40_000);
    }

    #[test]
    fn seek_target_backward_within_bounds() {
        assert_eq!(seek_target_ms(30_000, -10_000, 200_000), 20_000);
    }

    #[test]
    fn seek_target_backward_past_zero_clamps_to_zero() {
        assert_eq!(seek_target_ms(5_000, -10_000, 200_000), 0);
    }

    #[test]
    fn seek_target_forward_past_end_clamps_to_duration() {
        assert_eq!(seek_target_ms(195_000, 10_000, 200_000), 200_000);
    }

    #[test]
    fn seek_target_huge_position_clamps_to_duration_not_zero() {
        // A client position above i64::MAX must clamp to the END, never wrap
        // negative and land at 0 (release review 0.60.0).
        assert_eq!(seek_target_ms(u64::MAX, 0, 200_000), 200_000);
        assert_eq!(seek_target_ms(u64::MAX, -10_000, 200_000), 200_000);
    }

    #[test]
    fn seek_target_exact_zero_boundary() {
        assert_eq!(seek_target_ms(10_000, -10_000, 200_000), 0);
    }

    #[test]
    fn seek_target_exact_duration_boundary() {
        assert_eq!(seek_target_ms(190_000, 10_000, 200_000), 200_000);
    }

    #[test]
    fn seek_target_zero_delta_within_bounds_is_identity() {
        assert_eq!(seek_target_ms(50_000, 0, 200_000), 50_000);
    }

    #[test]
    fn seek_target_zero_delta_past_end_clamps_to_duration() {
        // The server route uses delta=0 purely to clamp a raw position to the
        // playing song's duration.
        assert_eq!(seek_target_ms(250_000, 0, 200_000), 200_000);
    }

    #[test]
    fn seek_target_zero_duration_yields_zero() {
        assert_eq!(seek_target_ms(50_000, 10_000, 0), 0);
    }

    // ---- seek_fraction: pos/dur clamped to 0.0..=1.0, 0.0 on zero duration --

    #[test]
    fn seek_fraction_quarter() {
        assert!((seek_fraction(50_000, 200_000) - 0.25).abs() < 1e-9);
    }

    #[test]
    fn seek_fraction_zero_position() {
        assert_eq!(seek_fraction(0, 200_000), 0.0);
    }

    #[test]
    fn seek_fraction_full() {
        assert_eq!(seek_fraction(200_000, 200_000), 1.0);
    }

    #[test]
    fn seek_fraction_zero_duration_is_zero_not_nan() {
        assert_eq!(seek_fraction(50_000, 0), 0.0);
    }

    #[test]
    fn seek_fraction_past_end_clamps_to_one() {
        assert_eq!(seek_fraction(250_000, 200_000), 1.0);
    }

    // ---- seek_display_ms: dragged value while dragging, else the live value --

    #[test]
    fn seek_display_dragging_returns_the_dragged_value() {
        // dragged != live so this also kills a "return live" mutant.
        assert_eq!(seek_display_ms(true, 100_000, 5_000), 100_000);
    }

    #[test]
    fn seek_display_not_dragging_returns_the_live_value() {
        // dragged != live so this also kills a "return dragged" mutant.
        assert_eq!(seek_display_ms(false, 100_000, 5_000), 5_000);
    }

    // ---- format_position: "M:SS", minutes may exceed 59 ----

    #[test]
    fn format_position_zero() {
        assert_eq!(format_position(0), "0:00");
    }

    #[test]
    fn format_position_seconds_pad() {
        assert_eq!(format_position(5_000), "0:05");
    }

    #[test]
    fn format_position_minute_and_seconds() {
        assert_eq!(format_position(65_000), "1:05");
    }

    #[test]
    fn format_position_ten_minutes() {
        assert_eq!(format_position(600_000), "10:00");
    }

    #[test]
    fn format_position_over_an_hour_keeps_minutes() {
        assert_eq!(format_position(3_661_000), "61:01");
    }

    #[test]
    fn format_position_truncates_sub_second() {
        assert_eq!(format_position(1_999), "0:01");
    }
}
