//! #207 — map a decoder frame-allocation failure to a DROPPED frame.
//!
//! `sp-decoder`'s `frame_pool::try_take` returns a typed
//! [`DecoderError::FrameAlloc`] when a per-frame video buffer cannot be
//! allocated (host out of commit). The SDK-clocked decode loop routes THAT error
//! here: it drops the frame (a bump of [`LoopStageMax::observe_alloc_drop`],
//! surfaced as `frames_dropped_alloc` in the `pipeline: loop-stats` line) and
//! emits a per-output rate-limited WARN, so the wall stutters instead of the
//! process aborting (`handle_alloc_error`, the #156 `0xc0000409` class). Any
//! OTHER decoder error keeps the existing abort-the-loop behaviour.
//!
//! The classification ([`frame_alloc_bytes`]) and the rate-limit gate
//! ([`should_warn`]) are pure + Linux-tested; only [`note_if_alloc_drop`] (the
//! process-global per-output warn map + the WARN) is integration glue.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use sp_decoder::DecoderError;

use crate::playback::loop_stats::LoopStageMax;

/// Minimum gap between per-output frame-alloc WARNs (once per 10 s per output),
/// so a commit-pressure storm logs at most once per output per window rather
/// than once per dropped frame.
const WARN_GAP: Duration = Duration::from_secs(10);

/// The per-output (`playlist_id`) last-WARN times.
static LAST_ALLOC_WARN: LazyLock<Mutex<HashMap<i64, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Pure: the requested byte count if `e` is a frame-allocation failure, else
/// `None`. The one place the pipeline decides "drop this frame" vs "abort".
pub(crate) fn frame_alloc_bytes(e: &DecoderError) -> Option<usize> {
    match e {
        DecoderError::FrameAlloc(bytes) => Some(*bytes),
        _ => None,
    }
}

/// Pure: should a WARN fire now, given the last WARN time for this output? Fires
/// when there is no previous WARN, or at least `min_gap` has elapsed.
pub(crate) fn should_warn(last: Option<Instant>, now: Instant, min_gap: Duration) -> bool {
    match last {
        None => true,
        Some(t) => now.duration_since(t) >= min_gap,
    }
}

/// #207: if `e` is a frame-allocation failure, record a dropped frame in
/// `loop_stage` (`frames_dropped_alloc`) and emit a per-output rate-limited WARN,
/// returning `true` (the caller SKIPS the frame — the loop simply continues). Any
/// other error → `false` (the caller keeps its existing abort behaviour).
/// Integration glue (process-global warn map + WARN); the decision pieces
/// ([`frame_alloc_bytes`] / [`should_warn`]) are unit-tested.
#[cfg_attr(not(windows), allow(dead_code))] // only caller is the #[cfg(windows)] decode loop
#[cfg_attr(test, mutants::skip)]
pub(crate) fn note_if_alloc_drop(
    e: &DecoderError,
    playlist_id: i64,
    loop_stage: &mut LoopStageMax,
) -> bool {
    let Some(bytes) = frame_alloc_bytes(e) else {
        return false;
    };
    loop_stage.observe_alloc_drop();
    let now = Instant::now();
    if let Ok(mut map) = LAST_ALLOC_WARN.lock() {
        if should_warn(map.get(&playlist_id).copied(), now, WARN_GAP) {
            map.insert(playlist_id, now);
            tracing::warn!(
                playlist_id,
                bytes,
                "frame buffer allocation failed — dropping frame (host out of commit; see the `host: commit` line)"
            );
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_alloc_bytes_matches_only_the_alloc_variant() {
        assert_eq!(
            frame_alloc_bytes(&DecoderError::FrameAlloc(1234)),
            Some(1234)
        );
        assert_eq!(frame_alloc_bytes(&DecoderError::Seek("x".into())), None);
        assert_eq!(frame_alloc_bytes(&DecoderError::Decode("y".into())), None);
        assert_eq!(
            frame_alloc_bytes(&DecoderError::BufferLock("z".into())),
            None
        );
    }

    #[test]
    fn should_warn_fires_first_time_then_respects_the_gap() {
        let gap = Duration::from_secs(10);
        let t0 = Instant::now();
        assert!(should_warn(None, t0, gap), "no previous WARN → fire");
        assert!(
            !should_warn(Some(t0), t0 + Duration::from_secs(9), gap),
            "within the gap → suppress"
        );
        assert!(
            should_warn(Some(t0), t0 + Duration::from_secs(10), gap),
            "exactly at the gap → fire (>= boundary)"
        );
        assert!(
            should_warn(Some(t0), t0 + Duration::from_secs(11), gap),
            "past the gap → fire"
        );
    }
}
