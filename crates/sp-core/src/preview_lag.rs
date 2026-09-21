//! Preview lag readout threshold (#184 round F).
//!
//! The browser shim measures how far the live preview PICTURE is behind the wall
//! (`produced_ms/1000 − buffered_end`, from the server's 1 Hz beacon). This is
//! the pure decision of WHEN that lag is worth showing the operator and WHAT
//! number to print. It lives in `sp_core` (WASM-safe) so the workspace Test job
//! and the diff-scoped mutation gate cover it — sp-ui has no unit-test job.

/// Show the "náhľad mešká N s" readout only once the picture is at least this
/// far behind the wall. Below it the preview is effectively live (a ~2 s
/// live-edge chase is normal) and a readout would be noise.
pub const PREVIEW_LAG_MIN_VISIBLE_S: f64 = f64::INFINITY;

/// The lag readout to render for a measured `lag_s`, or `None` when the preview
/// is live enough to hide it. Once the lag reaches
/// [`PREVIEW_LAG_MIN_VISIBLE_S`] it is rounded to whole seconds; a non-finite or
/// negative lag (the buffered end is AHEAD of the produced media — a startup
/// transient) hides the readout.
pub fn preview_lag_display(lag_s: f64) -> Option<i64> {
    if lag_s.is_finite() && lag_s >= PREVIEW_LAG_MIN_VISIBLE_S {
        Some(lag_s.round() as i64)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_below_3s_shown_at_and_above_rounded() {
        // Below the threshold → hidden.
        assert_eq!(preview_lag_display(0.0), None);
        assert_eq!(preview_lag_display(2.9), None);
        // Exactly at the threshold → shown as 3 s. Pins the `>=` boundary.
        assert_eq!(preview_lag_display(3.0), Some(3));
        // Rounds to whole seconds: 3.4 → 3 (kills a ceil mutant), 3.6 → 4 (kills
        // a floor/trunc mutant).
        assert_eq!(preview_lag_display(3.4), Some(3));
        assert_eq!(preview_lag_display(3.6), Some(4));
        assert_eq!(preview_lag_display(30.0), Some(30));
        // Non-finite / negative (buffered end ahead of produced media) → hidden.
        assert_eq!(preview_lag_display(-5.0), None);
        assert_eq!(preview_lag_display(f64::NAN), None);
        assert_eq!(preview_lag_display(f64::INFINITY), None);
    }
}
