//! Pipeline command + event enums, split out of `pipeline.rs` to keep that
//! file under the 1000-line cap (#196). Re-exported from `pipeline.rs`
//! (`pub use pipeline_types::{PipelineCommand, PipelineEvent}`) so every
//! existing `pipeline::PipelineCommand` / `pipeline::PipelineEvent` path — and
//! `super::*` in the `#[path]`-included test modules — still resolves.

use std::path::PathBuf;

/// Commands sent from the async engine to the pipeline thread.
#[derive(Debug)]
pub enum PipelineCommand {
    /// Start playing a song. Both the video sidecar (`.mp4`) and the audio
    /// sidecar (`.flac`) must exist. When `start_position_ms` is `Some(ms)`,
    /// the inner decode loop seeks to that offset BEFORE starting frame
    /// submission — atomic play-from-position eliminating the race between
    /// a separate Play+Seek dance (see issue #88).
    Play {
        video: PathBuf,
        audio: PathBuf,
        start_position_ms: Option<u64>,
    },
    /// Pause playback (send black frames).
    Pause,
    /// Resume playback after pause.
    Resume,
    /// Seek to the given ms offset within the current song. No-op if no
    /// song is currently loaded.
    Seek { position_ms: u64 },
    /// Stop playback entirely (send black, clear reader).
    Stop,
    /// Shut down the thread.
    Shutdown,
}

/// Events emitted by the pipeline thread back to the async engine.
// `HealthSnapshot` carries the full NDI/genlock/audio telemetry (#192 added the
// emitter stats) and is sent once per 5 s poll — its size is irrelevant on this
// channel, so boxing it would only add an allocation per snapshot.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum PipelineEvent {
    /// Video playback started; duration is known.
    Started { duration_ms: u64 },
    /// Periodic position update.
    Position { position_ms: u64, duration_ms: u64 },
    /// Video reached its natural end.
    Ended,
    /// An error occurred during playback.
    Error(String),
    /// Per-pipeline NDI health heartbeat. Emitted every ~5 seconds by the
    /// pipeline thread when running on Windows; consumed by
    /// `PlaybackEngine::handle_health_snapshot` (impl in
    /// `playback/ndi_health.rs`). The pipeline reports its locally-inferred
    /// state (Idle / Playing / Paused); the engine reconciles it against
    /// canonical `PlayState` before publishing to the dashboard.
    HealthSnapshot {
        connections: i32,
        frames_submitted_total: u64,
        frames_submitted_last_5s: u32,
        observed_fps: f32,
        nominal_fps: f32,
        /// `Instant` is fine on the wire here because emitter and consumer
        /// are in the same process. The engine maps it to `DateTime<Utc>`
        /// using a fixed `Instant`-to-`SystemTime` reference before
        /// publishing.
        last_submit_ts: Option<std::time::Instant>,
        last_heartbeat_ts: std::time::Instant,
        consecutive_bad_polls: u32,
        reported_state: crate::playback::ndi_health::PlaybackStateLabel,
        /// Boundary-paced emission telemetry (#147). Default (disabled, zeros)
        /// from the SDK-clocked / idle heartbeat paths; the paced decode loop
        /// fills it from the `Pacer`.
        pacing: crate::playback::ndi_health::PacingStats,
        /// Audio clock-discipline telemetry (#148); default off the SDK-clocked /
        /// idle paths, filled from the `Pacer`'s audio buffer + PLL when paced.
        audio: crate::playback::ndi_health::AudioStats,
    },
}
