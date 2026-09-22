//! #184 round G0.1 — dub-priority yield decisions for the stem worker.
//!
//! A dub job is an explicit operator request with a deadline; a background stem
//! separation is not. When a dub is queued behind the process-global heavy slot
//! (`heavy_slot::dub_slot_wanted()`), the stem worker (a) DEFERS its next tick
//! and (b) YIELDS a separation already running, so the dub acquires within ~1 s.
//!
//! The pure decision [`stem_tick_defers_to_dub`] is unit-tested exactly; the
//! mid-run yield (`yield_reason` + the watcher) is added alongside the stem
//! worker's separation seam.

/// Pure: whether a stem worker tick must be skipped because a dub is queued
/// behind the heavy slot. `true` while a dub waits, so the tick starts no new
/// separation (the row stays pending — no backoff, no DB write). The lyrics
/// worker's heavy tick defers on the same flag.
pub(crate) fn stem_tick_defers_to_dub(dub_wanted: bool) -> bool {
    dub_wanted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_queued_dub_defers_a_stem_tick() {
        assert!(
            stem_tick_defers_to_dub(true),
            "a dub queued behind the heavy slot defers the stem tick"
        );
        assert!(
            !stem_tick_defers_to_dub(false),
            "no dub queued → the stem worker proceeds"
        );
    }
}
