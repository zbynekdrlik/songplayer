//! Pacer scheduling + sink helpers split out of `pacer.rs` to keep it under the
//! 1000-line cap: the pure [`plan_sleep_100ns`] sleep decision (#147 change 4)
//! and the [`default_submit_shared`] `PacedSink::submit_shared` default (#203).
//! Re-exported from `pacer` so the original paths stay valid.

use sp_core::genlock::UNITS_PER_SECOND;
use sp_ndi::AudioFrame;

use crate::playback::frame_buf::SharedFrame;
use crate::playback::pacer::{PacedFrame, PacedSink};

/// The pure sleep-plan decision (#147 change 4), factored out so it is testable
/// without a live decode loop. `SystemClock` jumps and bad boundaries must never
/// park the send thread unboundedly, so the coarse sleep is clamped to
/// `[0, 1 s]` (camera-box `ndi.rs`), and a wall clock that sits MORE than one
/// interval before the boundary is a backward jump → `relatch` so the caller
/// re-latches instead of spin-waiting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SleepDecision {
    /// Coarse monotonic sleep in 100-ns units, clamped to `[0, 1 s]`.
    pub sleep_100ns: i64,
    /// The boundary sits more than one interval ahead of `now` — the clock
    /// stepped backward; do not spin, let the loop re-latch.
    pub relatch: bool,
}

/// Compute the [`SleepDecision`] for a wall clock at `now_100ns` targeting the
/// boundary `until_100ns` on a grid of `interval_100ns`. Pure; see
/// [`SleepDecision`].
pub fn plan_sleep_100ns(now_100ns: i64, until_100ns: i64, interval_100ns: i64) -> SleepDecision {
    let delta = until_100ns - now_100ns;
    SleepDecision {
        sleep_100ns: delta.clamp(0, UNITS_PER_SECOND),
        // The pacer only ever waits to the IMMEDIATE next boundary, so a normal
        // wait has `delta` within ONE grid slot. The exact-rational grid has ten
        // 333_334-wide slots per second (one tick wider than the nominal
        // `interval_100ns`, 333_333 @30 fps), so the bound is `interval + 2`
        // (slot width + a tick of margin) — NOT `> interval`, which mis-flagged
        // every wide slot as a backward jump and burned a self-clearing spin
        // (fix-lane-2 off-by-one). A larger gap is a genuine backward clock jump
        // (boundary far ahead). `interval == 0` (genlock off) never relatches —
        // it just sleeps the clamped 1 s.
        relatch: interval_100ns > 0 && delta > interval_100ns + 2,
    }
}

/// The default [`PacedSink::submit_shared`] body: build a one-shot [`PacedFrame`]
/// over the borrowed pixels and delegate to [`PacedSink::emit`], so a sink that
/// implements only `emit` (the test sinks) keeps working. [`FrameSubmitter`]
/// OVERRIDES `submit_shared` to move the [`SharedFrame`] straight into the
/// zero-copy async holdover instead.
///
/// [`FrameSubmitter`]: crate::playback::submitter::FrameSubmitter
#[allow(clippy::too_many_arguments)]
pub(crate) fn default_submit_shared<S: PacedSink + ?Sized>(
    sink: &mut S,
    width: u32,
    height: u32,
    stride: u32,
    video: SharedFrame,
    audio: &[AudioFrame],
    video_tc_100ns: i64,
    audio_tc_100ns: i64,
) {
    let frame = PacedFrame {
        pts_ns: 0,
        width,
        height,
        stride,
        video: video.to_vec(),
        audio: Vec::new(),
    };
    sink.emit(&frame, audio, video_tc_100ns, audio_tc_100ns);
}
