//! Pure, WASM-safe seek helpers (#194 ROUND 1).
//!
//! Shared by the sp-ui `Player` (the seek bar + ±10 s buttons + position
//! readout) and the `POST /api/v1/playback/{id}/seek` route (server-side clamp).
//! No I/O, no platform code — covered by the workspace `Test` job and the
//! diff-scoped mutation gate, so the boundary behaviour is verified once and
//! reused everywhere instead of re-derived per call site.

// NOTE (#194 RED): the implementations are added in the paired GREEN commit;
// this module ships its exact-boundary tests FIRST so the fix is proven to
// move them from red to green.

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
