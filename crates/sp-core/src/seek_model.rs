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
    // u64 math throughout: a position above i64::MAX must clamp to the end,
    // never wrap negative (`saturating_add_signed` floors at 0 on its own).
    current_ms.saturating_add_signed(delta_ms).min(duration_ms)
}

/// Progress fraction `pos_ms / dur_ms`, clamped to `0.0..=1.0`. Returns `0.0`
/// for a zero duration (no divide-by-zero, no `NaN`).
pub fn seek_fraction(pos_ms: u64, dur_ms: u64) -> f64 {
    if dur_ms == 0 {
        return 0.0;
    }
    (pos_ms as f64 / dur_ms as f64).clamp(0.0, 1.0)
}

/// A committed-but-not-yet-honoured seek. On release the operator's thumb landed
/// at `target_ms` (a `POST …/seek` fired) at wall time `committed_at_ms`, but the
/// pipeline's post-seek fast-forward has not yet delivered a frame near the
/// target, so the live position still reads the stale PRE-seek value for up to
/// ~3 s (#184). Held so the seek bar can DISPLAY the requested target across that
/// gap instead of visibly snapping back to the stale live position and then
/// forward once the fast-forward lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingSeek {
    /// The absolute position the seek requested (already clamped to duration).
    pub target_ms: u64,
    /// Wall-clock time the seek was committed, in the same monotonic clock passed
    /// as `now_ms` to `seek_display_ms`.
    pub committed_at_ms: u64,
}

/// Once the live position gets within this window of a pending seek's target the
/// pipeline has essentially caught up, so the honest live position drives the
/// display again (the pending hold releases).
pub const SEEK_CATCH_UP_MS: u64 = 1500;

/// A pending seek's display hold expires this long after the commit, so a seek
/// the pipeline cannot honour (e.g. at EOS, where the fast-forward never reaches
/// the target) falls back to the live position instead of showing an unreachable
/// target forever.
pub const SEEK_HOLD_MS: u64 = 5000;

/// The position a seek bar should DISPLAY (and bind to `prop:value`).
///
/// - While the operator is dragging the thumb, the dragged value is
///   authoritative so a live position tick can't snap it back — dragging wins
///   over everything, including a pending seek.
/// - Otherwise, while a committed `pending` seek is still catching up — the live
///   position is more than `SEEK_CATCH_UP_MS` behind the target AND fewer than
///   `SEEK_HOLD_MS` have elapsed since the commit — the committed TARGET is
///   displayed, so the bar never jumps back to the stale live position and then
///   forward after a seek (#184).
/// - Once the live position reaches `target − SEEK_CATCH_UP_MS` (inclusive) or
///   `SEEK_HOLD_MS` (inclusive) elapse, the pending no longer holds and the live
///   position drives the display again.
///
/// Pure so the gate's boundary behaviour is unit-tested (sp-ui has no unit-test
/// job).
pub fn seek_display_ms(
    dragging: bool,
    dragged_ms: u64,
    live_ms: u64,
    pending: Option<PendingSeek>,
    now_ms: u64,
) -> u64 {
    if dragging {
        return dragged_ms;
    }
    if let Some(p) = pending {
        if live_ms < p.target_ms.saturating_sub(SEEK_CATCH_UP_MS)
            && now_ms.saturating_sub(p.committed_at_ms) < SEEK_HOLD_MS
        {
            return p.target_ms;
        }
    }
    live_ms
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

    // ---- seek_display_ms: dragged value while dragging, else live, with a
    // pending-seek target hold (#184) ----

    #[test]
    fn seek_display_dragging_returns_the_dragged_value() {
        // dragged != live so this also kills a "return live" mutant.
        assert_eq!(seek_display_ms(true, 100_000, 5_000, None, 0), 100_000);
    }

    #[test]
    fn seek_display_dragging_wins_over_a_pending_seek() {
        // The drag is authoritative even while a pending seek would otherwise
        // hold — the operator's live finger beats the committed target.
        let p = PendingSeek {
            target_ms: 150_000,
            committed_at_ms: 1_000,
        };
        assert_eq!(
            seek_display_ms(true, 100_000, 5_000, Some(p), 2_000),
            100_000
        );
    }

    #[test]
    fn seek_display_not_dragging_no_pending_returns_the_live_value() {
        // dragged != live so this also kills a "return dragged" mutant.
        assert_eq!(seek_display_ms(false, 100_000, 5_000, None, 0), 5_000);
    }

    #[test]
    fn seek_display_no_pending_ignores_now_ms() {
        // With no pending seek the now_ms clock is irrelevant — behaviour is
        // identical to the pre-#184 two-input rule (dragging still wins).
        assert_eq!(seek_display_ms(false, 0, 42_000, None, 999_999), 42_000);
        assert_eq!(seek_display_ms(true, 7_000, 42_000, None, 999_999), 7_000);
    }

    #[test]
    fn seek_display_pending_stale_live_shows_the_target() {
        // The #184 fix: the live position is still 2_000 ms behind the committed
        // target (> SEEK_CATCH_UP_MS = 1500) and only 1 s has elapsed, so the bar
        // shows the requested TARGET, not the stale live position.
        let p = PendingSeek {
            target_ms: 100_000,
            committed_at_ms: 1_000,
        };
        assert_eq!(seek_display_ms(false, 0, 98_000, Some(p), 2_000), 100_000);
    }

    #[test]
    fn seek_display_pending_live_at_catch_up_boundary_is_live() {
        // live == target − SEEK_CATCH_UP_MS (1500) EXACTLY → the pipeline has
        // essentially caught up, so the honest live position drives the display
        // (the catch-up boundary is exclusive).
        let p = PendingSeek {
            target_ms: 100_000,
            committed_at_ms: 0,
        };
        assert_eq!(seek_display_ms(false, 0, 98_500, Some(p), 100), 98_500);
    }

    #[test]
    fn seek_display_pending_live_past_catch_up_window_is_live() {
        // The live position is only 1_000 ms behind (< 1500) → the fast-forward
        // is essentially there, show live.
        let p = PendingSeek {
            target_ms: 100_000,
            committed_at_ms: 0,
        };
        assert_eq!(seek_display_ms(false, 0, 99_000, Some(p), 100), 99_000);
    }

    #[test]
    fn seek_display_pending_just_before_hold_expiry_still_shows_target() {
        // 4_999 ms since commit (< SEEK_HOLD_MS = 5000) and live still far
        // behind → the target hold is still in effect.
        let p = PendingSeek {
            target_ms: 100_000,
            committed_at_ms: 1_000,
        };
        assert_eq!(seek_display_ms(false, 0, 10_000, Some(p), 5_999), 100_000);
    }

    #[test]
    fn seek_display_pending_hold_expiry_boundary_is_live() {
        // now − committed == SEEK_HOLD_MS (5000) EXACTLY → the hold has expired
        // even though the live position is still far behind (a seek the pipeline
        // could not honour), so it falls back to the live position (the hold
        // boundary is exclusive).
        let p = PendingSeek {
            target_ms: 100_000,
            committed_at_ms: 1_000,
        };
        assert_eq!(seek_display_ms(false, 0, 10_000, Some(p), 6_000), 10_000);
    }

    #[test]
    fn seek_display_pending_target_below_catch_up_window_shows_live() {
        // A near-start seek (target 1_000 < SEEK_CATCH_UP_MS) →
        // target.saturating_sub(1500) == 0, and no u64 live is < 0, so it never
        // holds (the live position is already at/after the target).
        let p = PendingSeek {
            target_ms: 1_000,
            committed_at_ms: 0,
        };
        assert_eq!(seek_display_ms(false, 0, 0, Some(p), 100), 0);
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
