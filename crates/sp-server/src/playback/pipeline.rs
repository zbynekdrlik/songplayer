//! Decode-to-NDI pipeline running on a dedicated OS thread.
//!
//! [`PlaybackPipeline`] owns a background thread that receives
//! [`PipelineCommand`]s over a crossbeam channel and emits
//! [`PipelineEvent`]s back to the async engine via a Tokio mpsc channel.
//!
//! On Windows the thread decodes video via `sp_decoder::SplitSyncedDecoder`
//! (driven by `MediaFoundationVideoReader` + `SymphoniaAudioReader`) and
//! sends frames over NDI.  On other platforms the thread logs a warning and
//! immediately reports an error (video decode requires Media Foundation).

use crossbeam_channel::{Receiver, Sender};
use std::path::PathBuf;
use std::thread;
use tracing::{info, warn};

// Used in cfg(windows) blocks:
#[cfg(windows)]
use crate::playback::submitter::FrameSubmitter;
#[cfg(windows)]
use crossbeam_channel::TryRecvError;
#[cfg(windows)]
use std::time::Instant;
#[cfg(windows)]
use tracing::{debug, error};

/// Commands sent from the async engine to the pipeline thread.
#[derive(Debug)]
pub enum PipelineCommand {
    /// Start playing a song. Both the video sidecar (`.mp4`) and the audio
    /// sidecar (`.flac`) must exist.
    Play { video: PathBuf, audio: PathBuf },
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
    },
}

/// Handle to a background decode-to-NDI pipeline thread.
pub struct PlaybackPipeline {
    cmd_tx: Sender<PipelineCommand>,
    handle: Option<thread::JoinHandle<()>>,
    ndi_name: String,
}

/// Shared NDI backend handle (Windows only). Wraps the loaded NDI SDK so
/// multiple pipeline threads can create senders without re-initializing.
#[cfg(windows)]
pub type SharedNdiBackend = std::sync::Arc<sp_ndi::RealNdiBackend>;

impl PlaybackPipeline {
    /// Spawn the decode-to-NDI loop on a dedicated OS thread.
    ///
    /// * `ndi_name` — NDI source name for this pipeline.
    /// * `ndi_backend` — shared NDI backend (Windows only, `None` on other platforms).
    /// * `event_tx` — channel for sending events back to the async engine.
    /// * `playlist_id` — used to tag events so the engine knows which playlist
    ///   they belong to.
    #[cfg(windows)]
    pub fn spawn(
        ndi_name: String,
        ndi_backend: Option<SharedNdiBackend>,
        event_tx: tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
        playlist_id: i64,
    ) -> Self {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();

        let ndi_name_for_self = ndi_name.clone();
        let handle = thread::Builder::new()
            .name(format!("pipeline-{playlist_id}"))
            .spawn(move || {
                run_loop(cmd_rx, &ndi_name, ndi_backend, event_tx, playlist_id);
            })
            .expect("failed to spawn pipeline thread");

        Self {
            cmd_tx,
            handle: Some(handle),
            ndi_name: ndi_name_for_self,
        }
    }

    /// Spawn the decode-to-NDI loop on a dedicated OS thread (non-Windows stub).
    #[cfg(not(windows))]
    pub fn spawn(
        ndi_name: String,
        _ndi_backend: Option<()>,
        event_tx: tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
        playlist_id: i64,
    ) -> Self {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();

        let ndi_name_for_self = ndi_name.clone();
        let handle = thread::Builder::new()
            .name(format!("pipeline-{playlist_id}"))
            .spawn(move || {
                run_loop(cmd_rx, &ndi_name, event_tx, playlist_id);
            })
            .expect("failed to spawn pipeline thread");

        Self {
            cmd_tx,
            handle: Some(handle),
            ndi_name: ndi_name_for_self,
        }
    }

    /// Send a command to the pipeline thread.
    pub fn send(&self, cmd: PipelineCommand) {
        let _ = self.cmd_tx.send(cmd);
    }

    /// Gracefully shut down the pipeline, blocking until the thread exits.
    pub fn shutdown(mut self) {
        let _ = self.cmd_tx.send(PipelineCommand::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// Borrow the NDI source name this pipeline was spawned with. Used by
    /// `playback::ndi_health` to populate health snapshot labels.
    pub fn ndi_name(&self) -> &str {
        &self.ndi_name
    }
}

impl Drop for PlaybackPipeline {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(PipelineCommand::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Main loop for the pipeline thread (non-Windows).
#[cfg(not(windows))]
fn run_loop(
    cmd_rx: Receiver<PipelineCommand>,
    ndi_name: &str,
    event_tx: tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
) {
    info!(ndi_name, playlist_id, "pipeline thread started");
    run_loop_stub(cmd_rx, ndi_name, event_tx, playlist_id);
    info!(playlist_id, "pipeline thread exited");
}

/// Main loop for the pipeline thread (Windows).
#[cfg(windows)]
fn run_loop(
    cmd_rx: Receiver<PipelineCommand>,
    ndi_name: &str,
    ndi_backend: Option<SharedNdiBackend>,
    event_tx: tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
) {
    info!(ndi_name, playlist_id, "pipeline thread started");
    run_loop_windows(cmd_rx, ndi_name, ndi_backend, event_tx, playlist_id);
    info!(playlist_id, "pipeline thread exited");
}

/// Non-Windows stub: waits for commands and reports errors for Play.
#[cfg(not(windows))]
fn run_loop_stub(
    cmd_rx: Receiver<PipelineCommand>,
    _ndi_name: &str,
    event_tx: tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
) {
    loop {
        match cmd_rx.recv() {
            Ok(PipelineCommand::Shutdown) | Err(_) => {
                info!(playlist_id, "pipeline thread shutting down");
                break;
            }
            Ok(PipelineCommand::Play { video, audio }) => {
                warn!(
                    ?video,
                    ?audio,
                    "video decode not available on this platform"
                );
                let _ = event_tx.send((
                    playlist_id,
                    PipelineEvent::Error("Video decode requires Windows (Media Foundation)".into()),
                ));
            }
            Ok(PipelineCommand::Pause) => {
                info!(playlist_id, "pipeline: paused (stub)");
            }
            Ok(PipelineCommand::Resume) => {
                info!(playlist_id, "pipeline: resumed (stub)");
            }
            Ok(PipelineCommand::Seek { position_ms }) => {
                // Seek is a no-op when no song is loaded. When loaded, forward
                // to the decoder and log on error — seek failures shouldn't kill
                // the pipeline (decoder recovers on the next Play).
                tracing::debug!(position_ms, "pipeline: seek ignored (no song loaded)");
            }
            Ok(PipelineCommand::Stop) => {
                info!(playlist_id, "pipeline: stopped (stub)");
            }
        }
    }
}

/// Windows decode-to-NDI loop.
///
/// cargo-mutants: skip — this function drives the real MF + NDI SDK decode
/// loop which depends on live Windows runtime state (Media Foundation COM
/// objects, NDI SDK function pointers). On the Linux mutation runner neither
/// stack is available, so mutations survive with no observable effect. The
/// cross-platform call-ordering logic is covered by FrameSubmitter's unit
/// tests in submitter.rs which the mutation runner does exercise.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
fn run_loop_windows(
    cmd_rx: Receiver<PipelineCommand>,
    ndi_name: &str,
    ndi_backend: Option<SharedNdiBackend>,
    event_tx: tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
) {
    let backend = match ndi_backend {
        Some(b) => b,
        None => {
            error!("no NDI backend provided");
            let _ = event_tx.send((
                playlist_id,
                PipelineEvent::Error("NDI SDK not available".into()),
            ));
            wait_for_shutdown(&cmd_rx, playlist_id);
            return;
        }
    };

    // clock_video = true lets NDI pace `send_video_async` on its internal
    // high-resolution clock. clock_audio stays false because we submit both
    // streams from a single thread; clocking both would deadlock on startup.
    let sender = match sp_ndi::NdiSender::new_with_clocking(backend, ndi_name, true, false) {
        Ok(s) => s,
        Err(e) => {
            error!(%e, "failed to create NDI sender");
            let _ = event_tx.send((
                playlist_id,
                PipelineEvent::Error(format!("Failed to create NDI sender: {e}")),
            ));
            wait_for_shutdown(&cmd_rx, playlist_id);
            return;
        }
    };

    info!(ndi_name, "NDI sender created with clock_video=true");

    // Initial black frame so the NDI source is visible immediately with a
    // sane default frame rate. The FrameSubmitter's frame rate is updated
    // per-file via `set_frame_rate` when real playback starts.
    let mut submitter = FrameSubmitter::new(sender, 30, 1);
    submitter.send_black_bgra(1920, 1080);

    let mut paused = false;
    let mut last_heartbeat = std::time::Instant::now();
    let mut consecutive_bad_polls: u32 = 0;

    loop {
        match cmd_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                run_heartbeat_outer(
                    &mut submitter,
                    &event_tx,
                    playlist_id,
                    paused,
                    &mut last_heartbeat,
                    &mut consecutive_bad_polls,
                );
                continue;
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                info!(playlist_id, "pipeline thread shutting down (cmd_rx closed)");
                submitter.flush();
                break;
            }
            Ok(PipelineCommand::Shutdown) => {
                info!(playlist_id, "pipeline thread shutting down");
                submitter.flush();
                break;
            }

            Ok(PipelineCommand::Play { video, audio }) => {
                info!(
                    playlist_id,
                    prev_paused = paused,
                    ?video,
                    ?audio,
                    "pipeline: Play received (paused -> false)"
                );
                // Inner loop: decode current song; on NewPlay, restart decode
                // with the new pair. Breaks out to outer loop on Ended/Stopped/
                // Error; returns true on Shutdown.
                let mut current_video = video;
                let mut current_audio = audio;
                let shutdown_requested = loop {
                    info!(
                        ?current_video,
                        ?current_audio,
                        playlist_id,
                        "starting playback"
                    );
                    paused = false;

                    match decode_and_send(
                        &cmd_rx,
                        &mut submitter,
                        &current_video,
                        &current_audio,
                        &event_tx,
                        playlist_id,
                        &mut paused,
                        &mut last_heartbeat,
                        &mut consecutive_bad_polls,
                    ) {
                        DecodeResult::Ended => {
                            paused = false;
                            info!(playlist_id, "video ended naturally");
                            submitter.send_black_bgra(1920, 1080);
                            let _ = event_tx.send((playlist_id, PipelineEvent::Ended));
                            break false;
                        }
                        DecodeResult::Stopped => {
                            paused = false;
                            info!(playlist_id, "playback stopped");
                            submitter.send_black_bgra(1920, 1080);
                            break false;
                        }
                        DecodeResult::Shutdown => {
                            info!(playlist_id, "shutdown during playback");
                            submitter.flush();
                            break true;
                        }
                        DecodeResult::NewPlay {
                            video: new_v,
                            audio: new_a,
                        } => {
                            info!(?new_v, ?new_a, playlist_id, "switching to new song");
                            current_video = new_v;
                            current_audio = new_a;
                            continue;
                        }
                        DecodeResult::Error(msg) => {
                            paused = false;
                            error!(playlist_id, %msg, "decode error");
                            submitter.send_black_bgra(1920, 1080);
                            let _ = event_tx.send((playlist_id, PipelineEvent::Error(msg)));
                            break false;
                        }
                    }
                };

                if shutdown_requested {
                    break;
                }
            }

            Ok(PipelineCommand::Pause) => {
                info!(
                    playlist_id,
                    prev_paused = paused,
                    "pipeline: Pause (paused -> true)"
                );
                paused = true;
            }
            Ok(PipelineCommand::Resume) => {
                info!(
                    playlist_id,
                    prev_paused = paused,
                    "pipeline: Resume (paused -> false)"
                );
                paused = false;
            }
            Ok(PipelineCommand::Seek { position_ms }) => {
                // Seek is a no-op when no song is loaded. When loaded, forward
                // to the decoder and log on error — seek failures shouldn't kill
                // the pipeline (decoder recovers on the next Play).
                tracing::debug!(position_ms, "pipeline: seek ignored (no song loaded)");
            }
            Ok(PipelineCommand::Stop) => {
                submitter.send_black_bgra(1920, 1080);
                debug!(playlist_id, "stopped (no active playback)");
            }
        }
    }
}

/// Result of the inner decode loop.
#[cfg(windows)]
enum DecodeResult {
    /// Video reached end of stream.
    Ended,
    /// Stop command received.
    Stopped,
    /// Shutdown command received — thread should exit.
    Shutdown,
    /// A new Play command arrived mid-playback.
    NewPlay { video: PathBuf, audio: PathBuf },
    /// Decoder error.
    Error(String),
}

/// Inner decode loop: open both sidecar files, read synced frames, send to NDI.
///
/// Returns when the video ends or a Stop/Shutdown/Play command is received.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
fn decode_and_send(
    cmd_rx: &Receiver<PipelineCommand>,
    submitter: &mut FrameSubmitter<sp_ndi::RealNdiBackend>,
    video_path: &std::path::Path,
    audio_path: &std::path::Path,
    event_tx: &tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
    paused: &mut bool,
    last_heartbeat: &mut std::time::Instant,
    consecutive_bad_polls: &mut u32,
) -> DecodeResult {
    use sp_decoder::{MediaFoundationVideoReader, SplitSyncedDecoder, SymphoniaAudioReader};

    let video_reader = match MediaFoundationVideoReader::open(video_path) {
        Ok(v) => v,
        Err(e) => {
            return DecodeResult::Error(format!(
                "failed to open video {}: {e}",
                video_path.display()
            ));
        }
    };
    let audio_reader = match SymphoniaAudioReader::open(audio_path) {
        Ok(a) => a,
        Err(e) => {
            return DecodeResult::Error(format!(
                "failed to open audio {}: {e}",
                audio_path.display()
            ));
        }
    };
    let mut decoder = match SplitSyncedDecoder::new(Box::new(video_reader), Box::new(audio_reader))
    {
        Ok(d) => d,
        Err(e) => {
            return DecodeResult::Error(format!("SplitSyncedDecoder::new failed: {e}"));
        }
    };

    // Apply the file's real frame rate to the submitter so NDI paces correctly.
    let (num, den) = decoder.frame_rate();
    submitter.set_frame_rate(num as i32, den as i32);

    // Report start. Duration is sample-accurate from the FLAC STREAMINFO so
    // it is always correct at open time — no more duration=0 bug.
    let _ = event_tx.send((
        playlist_id,
        PipelineEvent::Started {
            duration_ms: decoder.duration_ms(),
        },
    ));

    let mut last_position_report = Instant::now();
    let mut frame_count: u64 = 0;

    loop {
        // Check for commands between frames (non-blocking).
        match cmd_rx.try_recv() {
            Ok(PipelineCommand::Shutdown) => {
                submitter.flush();
                return DecodeResult::Shutdown;
            }
            Ok(PipelineCommand::Stop) => {
                submitter.flush();
                return DecodeResult::Stopped;
            }
            Ok(PipelineCommand::Play { video, audio }) => {
                submitter.flush();
                return DecodeResult::NewPlay { video, audio };
            }
            Ok(PipelineCommand::Pause) => {
                *paused = true;
                debug!(playlist_id, "paused during playback");
            }
            Ok(PipelineCommand::Resume) => {
                *paused = false;
                debug!(playlist_id, "resumed playback");
            }
            Ok(PipelineCommand::Seek { position_ms }) => {
                if let Err(e) = decoder.seek(position_ms) {
                    tracing::warn!(?e, position_ms, "pipeline: seek failed");
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                submitter.flush();
                return DecodeResult::Shutdown;
            }
        }

        if *paused {
            submitter.send_black_bgra(1920, 1080);
            std::thread::sleep(std::time::Duration::from_millis(100));
            continue;
        }

        match decoder.next_synced() {
            Ok(Some((video_frame, audio_frames))) => {
                let ndi_audio: Vec<sp_ndi::AudioFrame> = audio_frames
                    .into_iter()
                    .map(|af| sp_ndi::AudioFrame {
                        data: af.data,
                        channels: af.channels,
                        sample_rate: af.sample_rate,
                    })
                    .collect();

                let timestamp_ms = video_frame.timestamp_ms;
                submitter.submit_nv12(
                    video_frame.width,
                    video_frame.height,
                    video_frame.stride,
                    video_frame.data,
                    &ndi_audio,
                );

                if should_run_heartbeat(last_heartbeat.elapsed()) {
                    run_heartbeat_inner(
                        submitter,
                        event_tx,
                        playlist_id,
                        last_heartbeat,
                        consecutive_bad_polls,
                    );
                }

                frame_count += 1;

                if last_position_report.elapsed() >= std::time::Duration::from_millis(500) {
                    let _ = event_tx.send((
                        playlist_id,
                        PipelineEvent::Position {
                            position_ms: timestamp_ms,
                            duration_ms: decoder.duration_ms(),
                        },
                    ));
                    last_position_report = Instant::now();
                }
            }
            Ok(None) => {
                info!(playlist_id, frame_count, "video decode complete");
                submitter.flush();
                return DecodeResult::Ended;
            }
            Err(e) => {
                submitter.flush();
                return DecodeResult::Error(format!("Decode error at frame {frame_count}: {e}"));
            }
        }
    }
}

/// Wait for Shutdown command (used when NDI failed to load).
#[cfg(windows)]
fn wait_for_shutdown(cmd_rx: &Receiver<PipelineCommand>, playlist_id: i64) {
    loop {
        match cmd_rx.recv() {
            Ok(PipelineCommand::Shutdown) | Err(_) => {
                info!(playlist_id, "pipeline thread shutting down (no NDI)");
                break;
            }
            Ok(cmd) => {
                debug!(playlist_id, ?cmd, "ignoring command (NDI not available)");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (Linux-testable, no cfg-gate)
// ---------------------------------------------------------------------------

/// Pure predicate: should the pipeline thread run a heartbeat now?
/// Extracted so the timing rule is unit-testable without a live decode loop.
fn should_run_heartbeat(elapsed: std::time::Duration) -> bool {
    elapsed >= std::time::Duration::from_secs(5)
}

/// Pure predicate: is the just-completed poll a "bad poll" per the spec?
/// Used by the pipeline thread to bump or reset `consecutive_bad_polls`.
/// Branches (state guard, connections, fps, staleness) are individually
/// covered by `heartbeat_decision_tests::classify_bad_poll_*` so the
/// mutation runner can validate every boundary.
fn classify_bad_poll(
    state: &crate::playback::ndi_health::PlaybackStateLabel,
    connections: i32,
    observed_fps: f32,
    nominal_fps: f32,
    last_submit_ts: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    if !matches!(
        state,
        crate::playback::ndi_health::PlaybackStateLabel::Playing
    ) {
        return false;
    }
    if connections == 0 {
        return true;
    }
    if nominal_fps > 0.0 && observed_fps < nominal_fps / 2.0 {
        return true;
    }
    if let Some(ts) = last_submit_ts {
        if now.duration_since(ts) > std::time::Duration::from_secs(10) {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Windows heartbeat helpers
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn run_heartbeat_outer(
    submitter: &mut FrameSubmitter<sp_ndi::RealNdiBackend>,
    event_tx: &tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
    paused: bool,
    last_heartbeat: &mut std::time::Instant,
    consecutive_bad_polls: &mut u32,
) {
    let state = if paused {
        crate::playback::ndi_health::PlaybackStateLabel::Paused
    } else {
        crate::playback::ndi_health::PlaybackStateLabel::Idle
    };
    emit_heartbeat(
        submitter,
        event_tx,
        playlist_id,
        state,
        last_heartbeat,
        consecutive_bad_polls,
    );
}

#[cfg(windows)]
fn run_heartbeat_inner(
    submitter: &mut FrameSubmitter<sp_ndi::RealNdiBackend>,
    event_tx: &tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
    last_heartbeat: &mut std::time::Instant,
    consecutive_bad_polls: &mut u32,
) {
    emit_heartbeat(
        submitter,
        event_tx,
        playlist_id,
        crate::playback::ndi_health::PlaybackStateLabel::Playing,
        last_heartbeat,
        consecutive_bad_polls,
    );
}

// mutants::skip — drives the live MF + NDI SDK heartbeat path; cross-platform
// behaviour is verified by the pure helpers should_run_heartbeat /
// classify_bad_poll plus the engine-side ndi_health tests using synthetic
// HealthSnapshot events. Same status as run_loop_windows.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
fn emit_heartbeat(
    submitter: &mut FrameSubmitter<sp_ndi::RealNdiBackend>,
    event_tx: &tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
    state: crate::playback::ndi_health::PlaybackStateLabel,
    last_heartbeat: &mut std::time::Instant,
    consecutive_bad_polls: &mut u32,
) {
    let connections = submitter.sender().get_no_connections(0);
    let stats = submitter.drain_window();
    let observed_fps = stats.frames_in_window as f32 / stats.window_secs.max(0.001);
    let nominal_fps = submitter.nominal_fps();

    let now = std::time::Instant::now();
    let bad = classify_bad_poll(
        &state,
        connections,
        observed_fps,
        nominal_fps,
        submitter.last_submit_ts(),
        now,
    );
    if bad {
        *consecutive_bad_polls = consecutive_bad_polls.saturating_add(1);
    } else {
        *consecutive_bad_polls = 0;
    }

    let _ = event_tx.send((
        playlist_id,
        PipelineEvent::HealthSnapshot {
            connections,
            frames_submitted_total: submitter.frames_submitted_total(),
            frames_submitted_last_5s: stats.frames_in_window,
            observed_fps,
            nominal_fps,
            last_submit_ts: submitter.last_submit_ts(),
            last_heartbeat_ts: now,
            consecutive_bad_polls: *consecutive_bad_polls,
            reported_state: state,
        },
    ));
    *last_heartbeat = now;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playback::ndi_health::PlaybackStateLabel;
    use std::time::Instant;

    #[test]
    fn pipeline_spawn_and_shutdown() {
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let pipeline = PlaybackPipeline::spawn("test-ndi".into(), None, event_tx, 1);
        pipeline.shutdown();
        // If we get here, the thread joined successfully.
    }

    #[test]
    fn pipeline_drop_sends_shutdown() {
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        {
            let _pipeline = PlaybackPipeline::spawn("test-drop".into(), None, event_tx, 2);
            // Pipeline dropped here — Drop impl should send Shutdown and join.
        }
        // If we get here without hanging, the Drop worked correctly.
    }

    #[test]
    fn pipeline_send_command_before_shutdown() {
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let pipeline = PlaybackPipeline::spawn("test-cmd".into(), None, event_tx, 3);
        pipeline.send(PipelineCommand::Stop);
        pipeline.send(PipelineCommand::Pause);
        pipeline.send(PipelineCommand::Resume);
        pipeline.shutdown();
    }

    #[test]
    fn pipeline_play_emits_event_on_non_windows() {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let pipeline = PlaybackPipeline::spawn("test-play".into(), None, event_tx, 4);

        pipeline.send(PipelineCommand::Play {
            video: PathBuf::from("/tmp/test_video.mp4"),
            audio: PathBuf::from("/tmp/test_audio.flac"),
        });
        // Give the thread a moment to process.
        std::thread::sleep(std::time::Duration::from_millis(50));
        pipeline.shutdown();

        // On non-Windows, we expect an Error event.
        #[cfg(not(windows))]
        {
            let (id, event) = event_rx.try_recv().expect("should have received an event");
            assert_eq!(id, 4);
            match event {
                PipelineEvent::Error(msg) => {
                    assert!(msg.contains("Windows"), "error should mention Windows");
                }
                other => panic!("expected Error event, got {other:?}"),
            }
        }

        let _ = event_rx;
    }

    #[test]
    fn seek_variant_carries_position_ms() {
        let cmd = PipelineCommand::Seek { position_ms: 12345 };
        match cmd {
            PipelineCommand::Seek { position_ms } => assert_eq!(position_ms, 12345),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn seek_does_not_collide_with_other_variants() {
        // Compile-time check that every variant is still distinct.
        let variants = vec![
            PipelineCommand::Play {
                video: PathBuf::new(),
                audio: PathBuf::new(),
            },
            PipelineCommand::Pause,
            PipelineCommand::Resume,
            PipelineCommand::Seek { position_ms: 0 },
            PipelineCommand::Stop,
            PipelineCommand::Shutdown,
        ];
        assert_eq!(variants.len(), 6);
    }

    #[test]
    fn pipeline_send_seek_command() {
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let pipeline = PlaybackPipeline::spawn("test-seek".into(), None, event_tx, 6);
        pipeline.send(PipelineCommand::Seek { position_ms: 5000 });
        pipeline.shutdown();
        // No panic or hang means the Seek arm is handled in the loop.
    }

    /// Regression test for the NewPlay bug: the outer loop must continue to
    /// receive and process subsequent Play commands after the first one returns.
    /// Before the fix, NewPlay was discarded and the worker would hang waiting
    /// for the next command instead of playing the new file.
    ///
    /// On non-Windows the stub emits an Error per Play. On Windows the loop
    /// path is structurally the same — verified live on win-resolume v0.7.2.
    #[test]
    fn pipeline_processes_multiple_sequential_plays() {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let pipeline = PlaybackPipeline::spawn("test-multi-play".into(), None, event_tx, 5);

        pipeline.send(PipelineCommand::Play {
            video: PathBuf::from("/tmp/song-a_video.mp4"),
            audio: PathBuf::from("/tmp/song-a_audio.flac"),
        });
        std::thread::sleep(std::time::Duration::from_millis(30));
        pipeline.send(PipelineCommand::Play {
            video: PathBuf::from("/tmp/song-b_video.mp4"),
            audio: PathBuf::from("/tmp/song-b_audio.flac"),
        });
        std::thread::sleep(std::time::Duration::from_millis(30));
        pipeline.send(PipelineCommand::Play {
            video: PathBuf::from("/tmp/song-c_video.mp4"),
            audio: PathBuf::from("/tmp/song-c_audio.flac"),
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        pipeline.shutdown();

        // Drain all events. We expect at least one event per Play command —
        // proving the worker did not hang after the first Play.
        #[cfg(not(windows))]
        {
            let mut event_count = 0;
            while let Ok((id, event)) = event_rx.try_recv() {
                assert_eq!(id, 5);
                match event {
                    PipelineEvent::Error(_) => event_count += 1,
                    other => panic!("unexpected event: {other:?}"),
                }
            }
            assert_eq!(
                event_count, 3,
                "expected 3 Error events (one per Play), got {event_count}"
            );
        }

        let _ = event_rx;
    }

    /// Pin: `PipelineCommand::Play` must reset `paused = false` BEFORE
    /// entering `decode_and_send`, so a stale `Pause` cannot leak across
    /// video changes. Static check via `include_str!` — fires red if the
    /// `paused = false` statement is moved out of the Play arm, deleted,
    /// or commented out.
    #[test]
    fn play_command_clears_paused_state() {
        let src = include_str!("pipeline.rs");
        let play_arm_start = src
            .find("Ok(PipelineCommand::Play {")
            .expect("Play arm must exist");
        let decode_call = src[play_arm_start..]
            .find("decode_and_send(")
            .expect("Play arm must call decode_and_send");
        let play_block = &src[play_arm_start..play_arm_start + decode_call];

        // Strict match: an actual statement (semicolon-terminated), not a
        // commented-out line or a docstring mention. Strip any line whose
        // first non-whitespace chars are `//` before checking.
        let live_lines: String = play_block
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            live_lines.contains("paused = false;"),
            "PipelineCommand::Play must clear `paused = false;` (live statement, not \
             a comment) BEFORE decode_and_send. Current Play arm:\n{play_block}"
        );
    }

    #[test]
    fn health_snapshot_variant_constructs_and_clones() {
        let now = Instant::now();
        let ev = PipelineEvent::HealthSnapshot {
            connections: 1,
            frames_submitted_total: 100,
            frames_submitted_last_5s: 30,
            observed_fps: 29.97,
            nominal_fps: 29.97,
            last_submit_ts: Some(now),
            last_heartbeat_ts: now,
            consecutive_bad_polls: 0,
            reported_state: PlaybackStateLabel::Playing,
        };
        let cloned = ev.clone();
        // Pattern-match to assert the variant exists and the fields round-trip.
        if let PipelineEvent::HealthSnapshot {
            connections,
            frames_submitted_last_5s,
            reported_state,
            ..
        } = cloned
        {
            assert_eq!(connections, 1);
            assert_eq!(frames_submitted_last_5s, 30);
            assert_eq!(reported_state, PlaybackStateLabel::Playing);
        } else {
            panic!("clone produced wrong variant");
        }
    }
}

#[cfg(test)]
#[path = "pipeline_heartbeat_tests.rs"]
mod heartbeat_decision_tests;
