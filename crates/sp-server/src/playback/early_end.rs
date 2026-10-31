//! #277: a video that ends long before its own duration is a FAILURE, never
//! the song's natural end (pure, Linux-tested, mutation-gated).
//!
//! Media Foundation's H.264 decoder stops on "late SEI" access units (SEI
//! NAL units after the slice data, as some YouTube streams carry them): on
//! SNV it handed out no picture of two SP-slow songs and reported
//! end-of-stream. The paced pipeline took that for the song's end, so the
//! song was skipped with an audio underrun at every play, silently.
//!
//! The decode producer measures where the picture stopped against the video
//! reader's own duration ([`EarlyEnd::of`]); an early end goes out as a
//! pipeline `Error` whose text starts with [`EARLY_END`], counted by #229's
//! open-failure handling, and the engine asks the video-upgrade worker to
//! repair the file (`video_upgrade::repair`).
//!
//! The pipeline answers each Play exactly once (`failure_backoff::PlayAnswers`).
//! A video that ends before its first picture fails before the emit loop
//! sends `Started`, so its `Error` is the Play's answer, like a failed open.
//! One that stops after some pictures fails after `Started`: that `Error`
//! carries [`after_start`]'s suffix, and the engine counts it as no answer
//! ([`is_after_start`]).

/// How far before the video's own duration its picture may end and still be
/// its natural end: a container's duration and its last picture's time
/// differ by a frame or two, never by seconds.
pub const EARLY_EOS_SLACK_MS: u64 = 10_000;

/// The text every early end's `Error` starts with.
pub const EARLY_END: &str = "early_end:";

/// The suffix of an early end's `Error` sent after its song's `Started`.
const AFTER_START: &str = " (after its start)";

/// Where a video's picture stopped and how long the video's own reader says
/// it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EarlyEnd {
    /// The position (ms) of the last picture handed out, else where the
    /// video played from.
    pub at_ms: u64,
    /// The video reader's duration (ms).
    pub duration_ms: u64,
}

impl EarlyEnd {
    /// The video ended at `at_ms` and its reader says it is `duration_ms`
    /// long: an early end when that is more than [`EARLY_EOS_SLACK_MS`]
    /// before its duration. A video of unknown length (0) never ends early.
    pub fn of(at_ms: u64, duration_ms: u64) -> Option<Self> {
        let reached = at_ms.saturating_add(EARLY_EOS_SLACK_MS);
        (reached < duration_ms).then_some(Self { at_ms, duration_ms })
    }

    /// The pipeline `Error`'s text for the video file `file` (its name).
    pub fn reason(&self, file: &str) -> String {
        format!(
            "{EARLY_END} {file} ended at {} ms of {} ms",
            self.at_ms, self.duration_ms
        )
    }
}

/// An early end's `reason` as sent after its song's `Started`: no answer to
/// a Play.
pub fn after_start(reason: &str) -> String {
    format!("{reason}{AFTER_START}")
}

/// Whether a pipeline `Error`'s text is an early end.
pub fn is_early_end(error: &str) -> bool {
    error.starts_with(EARLY_END)
}

/// Whether a pipeline `Error`'s text is an early end sent after its song's
/// `Started` ([`after_start`]).
pub fn is_after_start(error: &str) -> bool {
    is_early_end(error) && error.ends_with(AFTER_START)
}

#[cfg(test)]
#[path = "early_end_tests.rs"]
mod tests;
