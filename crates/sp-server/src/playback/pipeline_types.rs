//! Pipeline command + event enums, split out of `pipeline.rs` to keep that
//! file under the 1000-line cap (#196). Re-exported from `pipeline.rs`
//! (`pub use pipeline_types::{PipelineCommand, PipelineEvent}`) so every
//! existing `pipeline::PipelineCommand` / `pipeline::PipelineEvent` path — and
//! `super::*` in the `#[path]`-included test modules — still resolves.

use std::path::PathBuf;

use tracing::{info, warn};

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
// `HealthSnapshot` carries the full genlock/audio telemetry and is sent once
// per 5 s poll — its size is irrelevant on this channel, so boxing it would
// only add an allocation per snapshot.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum PipelineEvent {
    /// Video playback started; duration is known. `position_ms` is where
    /// the song really starts (#217): the Play's `start_position_ms` when
    /// the decoder's seek there worked, 0 when it failed (the song plays
    /// from its start) or none was asked. The song's title clock counts
    /// from it.
    Started { duration_ms: u64, position_ms: u64 },
    /// #217: a seek took effect, and the song plays on from `position_ms`
    /// (`real_seek_ms`): the asked position when the decoder's seek worked,
    /// where the decoder was when it refused it. The song's title clock
    /// moves on this report, never on the asked position (`seek.rs`). Sent
    /// by the decode producer right after its seek, so it always comes after
    /// the song's `Started` and before the next song's.
    Seeked { position_ms: u64 },
    /// Periodic position update.
    Position { position_ms: u64, duration_ms: u64 },
    /// Video reached its natural end.
    Ended,
    /// An error occurred during playback.
    Error(String),
    /// Per-pipeline health heartbeat. Emitted every ~5 seconds by the
    /// pipeline thread when running on Windows; consumed by
    /// `PlaybackEngine::handle_health_snapshot` (impl in
    /// `playback/ndi_health.rs`). The pipeline reports its locally-inferred
    /// state (Idle / Playing / Paused); the engine reconciles it against
    /// canonical `PlayState` before publishing to the dashboard. #221 lane 3:
    /// a playlist has no NDI output of its own, so there is no receiver count;
    /// the frame counts are the boundaries its paced output delivered to the
    /// program bus.
    HealthSnapshot {
        frames_submitted_total: u64,
        frames_submitted_last_5s: u32,
        observed_fps: f32,
        nominal_fps: f32,
        /// #168 round 6b: the decoder's SOURCE frame rate (`decoder.frame_rate()`
        /// as fps). Distinct from `nominal_fps`, which is the OUTPUT nominal —
        /// the fixed genlock grid. The lock rule needs the source rate to know
        /// the structural fps-conversion repeat fraction, so it reads THIS,
        /// never `nominal_fps` (which reads the grid 30 and falsely degraded a
        /// 24-fps output).
        source_fps: f32,
        /// `Instant` is fine on the wire here because emitter and consumer
        /// are in the same process. The engine maps it to `DateTime<Utc>`
        /// using a fixed `Instant`-to-`SystemTime` reference before
        /// publishing.
        last_submit_ts: Option<std::time::Instant>,
        last_heartbeat_ts: std::time::Instant,
        consecutive_bad_polls: u32,
        reported_state: crate::playback::ndi_health::PlaybackStateLabel,
        /// Boundary-paced emission telemetry (#147), from the `Pacer` merged
        /// with the paced output's counters.
        pacing: crate::playback::ndi_health::PacingStats,
        /// Audio clock-discipline telemetry (#148), from the `Pacer`'s audio
        /// buffer.
        audio: crate::playback::ndi_health::AudioStats,
        /// #147 round 9: SongPlayer's own memory residency for the per-minute
        /// `pipeline: loop-stats` line.
        loop_stats: crate::playback::loop_stats::LoopStats,
    },
    /// #215: not from a pipeline thread — the engine's own deferred
    /// scene-go-off pause of a playlist held through a program transition is
    /// due for a re-check (`scene_off.rs`). It carries the hold's re-check
    /// id: only the pending re-check acts, any other is stale.
    SceneOffDue(u64),
    /// #229: not from a pipeline thread — the pause after failed opens in a
    /// row is over (`failure_retry.rs`). It carries the retry's id: only the
    /// pending retry acts, any other is stale.
    RetryDue(u64),
    /// #221 L4b: not from a pipeline thread — the playback authority
    /// (`program_authority.rs`): the playlist went on air (`true`) or left
    /// it (`false`). The engine drops it when it is stale.
    OnProgram(bool),
}

/// #217: where a Play really starts, the position its `Started` reports:
/// `start_position_ms` when the decoder's `seek` there worked; 0 when it
/// failed (the song then plays from its start: [`real_seek_ms`] from 0) or
/// no position was asked (no seek is made). `who` names the decode path in
/// its log line (the paced producer).
pub fn real_start_ms<E: std::fmt::Debug>(
    start_position_ms: Option<u64>,
    seek: impl FnOnce(u64) -> Result<(), E>,
    playlist_id: i64,
    who: &str,
) -> u64 {
    start_position_ms.map_or(0, |ms| real_seek_ms(ms, 0, seek, playlist_id, who))
}

/// #217: where the song plays from after the decoder is asked to seek to
/// `position_ms`, the position `PipelineEvent::Seeked` reports: the asked
/// position when the `seek` worked; `current_ms` when it failed, since the
/// decoder then plays on from where it was (a song just opened is at 0).
/// `who` names the decode path in its log line.
pub fn real_seek_ms<E: std::fmt::Debug>(
    position_ms: u64,
    current_ms: u64,
    seek: impl FnOnce(u64) -> Result<(), E>,
    playlist_id: i64,
    who: &str,
) -> u64 {
    match seek(position_ms) {
        Ok(()) => {
            info!(playlist_id, position_ms, "{who}: seeked");
            position_ms
        }
        Err(e) => {
            warn!(
                playlist_id,
                position_ms,
                current_ms,
                ?e,
                "{who}: the decoder refused the seek — playing on from where it was"
            );
            current_ms
        }
    }
}

#[cfg(test)]
#[path = "pipeline_types_tests.rs"]
mod tests;
