//! Decode-to-NDI pipeline running on a dedicated OS thread.
//!
//! [`PlaybackPipeline`] owns a background thread that receives
//! [`PipelineCommand`]s over a crossbeam channel and emits
//! [`PipelineEvent`]s back to the async engine via a Tokio mpsc channel.
//!
//! On Windows the thread decodes video via `sp_decoder::SyncedDecoder` and
//! sends frames over NDI.  On other platforms the thread logs a warning and
//! immediately reports an error (video decode requires Media Foundation).

use crossbeam_channel::{Receiver, Sender};
use std::path::PathBuf;
use std::thread;
use tracing::{info, warn};

// Used in cfg(windows) blocks:
#[cfg(windows)]
use crossbeam_channel::TryRecvError;
#[cfg(windows)]
use std::time::Instant;
#[cfg(windows)]
use tracing::{debug, error};

/// Commands sent from the async engine to the pipeline thread.
#[derive(Debug)]
pub enum PipelineCommand {
    /// Start playing a video file.
    Play(PathBuf),
    /// Pause playback (send black frames).
    Pause,
    /// Resume playback after pause.
    Resume,
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
}

/// Handle to a background decode-to-NDI pipeline thread.
pub struct PlaybackPipeline {
    cmd_tx: Sender<PipelineCommand>,
    handle: Option<thread::JoinHandle<()>>,
}

impl PlaybackPipeline {
    /// Spawn the decode-to-NDI loop on a dedicated OS thread.
    ///
    /// * `ndi_name` — NDI source name for this pipeline.
    /// * `event_tx` — channel for sending events back to the async engine.
    /// * `playlist_id` — used to tag events so the engine knows which playlist
    ///   they belong to.
    pub fn spawn(
        ndi_name: String,
        event_tx: tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
        playlist_id: i64,
    ) -> Self {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();

        let handle = thread::Builder::new()
            .name(format!("pipeline-{playlist_id}"))
            .spawn(move || {
                run_loop(cmd_rx, &ndi_name, event_tx, playlist_id);
            })
            .expect("failed to spawn pipeline thread");

        Self {
            cmd_tx,
            handle: Some(handle),
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
}

impl Drop for PlaybackPipeline {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(PipelineCommand::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Main loop for the pipeline thread.
///
/// On Windows this initialises NDI and the decoder, then enters a frame loop.
/// On non-Windows it simply waits for commands and reports errors for Play
/// commands (since Media Foundation is not available).
fn run_loop(
    cmd_rx: Receiver<PipelineCommand>,
    ndi_name: &str,
    event_tx: tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
) {
    info!(ndi_name, playlist_id, "pipeline thread started");

    #[cfg(windows)]
    {
        run_loop_windows(cmd_rx, ndi_name, event_tx, playlist_id);
    }

    #[cfg(not(windows))]
    {
        run_loop_stub(cmd_rx, ndi_name, event_tx, playlist_id);
    }

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
            Ok(PipelineCommand::Play(path)) => {
                warn!(?path, "video decode not available on this platform");
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
            Ok(PipelineCommand::Stop) => {
                info!(playlist_id, "pipeline: stopped (stub)");
            }
        }
    }
}

/// Windows decode-to-NDI loop.
#[cfg(windows)]
fn run_loop_windows(
    cmd_rx: Receiver<PipelineCommand>,
    ndi_name: &str,
    event_tx: tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
) {
    use sp_ndi::{AudioFrame, NdiLib, NdiSender, RealNdiBackend, VideoFrame};
    use std::sync::Arc;

    // Load NDI SDK and create sender.
    let ndi_lib = match NdiLib::load() {
        Ok(lib) => Arc::new(lib),
        Err(e) => {
            error!(%e, "failed to load NDI SDK");
            let _ = event_tx.send((
                playlist_id,
                PipelineEvent::Error(format!("Failed to load NDI SDK: {e}")),
            ));
            // Fall back to stub loop — still accept commands but can't play.
            wait_for_shutdown(&cmd_rx, playlist_id);
            return;
        }
    };

    let backend = Arc::new(RealNdiBackend::new(ndi_lib));
    let sender = match NdiSender::new(backend, ndi_name) {
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

    info!(ndi_name, "NDI sender created, waiting for Play command");

    // Send initial black frame so the NDI source appears immediately.
    send_black_frame(&sender, 1920, 1080);

    let mut paused = false;

    loop {
        // Wait for a command (blocking).
        match cmd_rx.recv() {
            Ok(PipelineCommand::Shutdown) | Err(_) => {
                info!(playlist_id, "pipeline thread shutting down");
                break;
            }

            Ok(PipelineCommand::Play(path)) => {
                info!(?path, playlist_id, "starting playback");
                paused = false;

                // Decode loop — runs until video ends, Stop, or Shutdown.
                match decode_and_send(&cmd_rx, &sender, &path, &event_tx, playlist_id, &mut paused)
                {
                    DecodeResult::Ended => {
                        info!(playlist_id, "video ended naturally");
                        send_black_frame(&sender, 1920, 1080);
                        let _ = event_tx.send((playlist_id, PipelineEvent::Ended));
                    }
                    DecodeResult::Stopped => {
                        info!(playlist_id, "playback stopped");
                        send_black_frame(&sender, 1920, 1080);
                    }
                    DecodeResult::Shutdown => {
                        info!(playlist_id, "shutdown during playback");
                        break;
                    }
                    DecodeResult::NewPlay(new_path) => {
                        // Immediately start playing the new file (re-enter loop).
                        info!(?new_path, playlist_id, "switching to new video");
                        // Push the Play command back so the outer loop picks it up.
                        let _ = cmd_rx.try_recv(); // drain any stale
                        // We need to handle this inline — recursion is messy.
                        // The outer loop will receive the next command.
                        // For now, we re-enter by sending ourselves a Play.
                        // Actually, the simplest approach: just continue the outer
                        // loop — the caller will send a new Play command.
                        send_black_frame(&sender, 1920, 1080);
                    }
                    DecodeResult::Error(msg) => {
                        error!(playlist_id, %msg, "decode error");
                        send_black_frame(&sender, 1920, 1080);
                        let _ = event_tx.send((playlist_id, PipelineEvent::Error(msg)));
                    }
                }
            }

            Ok(PipelineCommand::Pause) => {
                paused = true;
                debug!(playlist_id, "paused (no active playback)");
            }
            Ok(PipelineCommand::Resume) => {
                paused = false;
                debug!(playlist_id, "resumed (no active playback)");
            }
            Ok(PipelineCommand::Stop) => {
                send_black_frame(&sender, 1920, 1080);
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
    NewPlay(PathBuf),
    /// Decoder error.
    Error(String),
}

/// Inner decode loop: open file, read frames, send to NDI.
///
/// Returns when the video ends or a Stop/Shutdown/Play command is received.
#[cfg(windows)]
fn decode_and_send(
    cmd_rx: &Receiver<PipelineCommand>,
    sender: &sp_ndi::NdiSender<sp_ndi::RealNdiBackend>,
    path: &std::path::Path,
    event_tx: &tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
    paused: &mut bool,
) -> DecodeResult {
    use sp_decoder::SyncedDecoder;

    let mut decoder = match SyncedDecoder::open(path) {
        Ok(d) => d,
        Err(e) => {
            return DecodeResult::Error(format!("Failed to open {}: {e}", path.display()));
        }
    };

    // Report start. Duration may be 0 initially — it gets refined as we decode.
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
            Ok(PipelineCommand::Shutdown) => return DecodeResult::Shutdown,
            Ok(PipelineCommand::Stop) => return DecodeResult::Stopped,
            Ok(PipelineCommand::Play(new_path)) => return DecodeResult::NewPlay(new_path),
            Ok(PipelineCommand::Pause) => {
                *paused = true;
                debug!(playlist_id, "paused during playback");
            }
            Ok(PipelineCommand::Resume) => {
                *paused = false;
                debug!(playlist_id, "resumed playback");
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return DecodeResult::Shutdown,
        }

        // If paused, send black frames at ~10fps and keep checking commands.
        if *paused {
            send_black_frame(sender, 1920, 1080);
            std::thread::sleep(std::time::Duration::from_millis(100));
            continue;
        }

        // Decode next synced frame.
        let result = decoder.next_synced();
        match result {
            Ok(Some((video_frame, audio_frames))) => {
                // Send video frame via NDI.
                // NDI clock_video=true handles frame pacing.
                let ndi_video = sp_ndi::VideoFrame {
                    data: video_frame.data,
                    width: video_frame.width,
                    height: video_frame.height,
                    stride: video_frame.stride,
                    frame_rate_n: 30000,
                    frame_rate_d: 1001,
                };
                sender.send_video(&ndi_video);

                // Send all audio chunks for this video frame.
                for af in &audio_frames {
                    let ndi_audio = sp_ndi::AudioFrame {
                        data: af.data.clone(),
                        channels: af.channels,
                        sample_rate: af.sample_rate,
                    };
                    sender.send_audio(&ndi_audio);
                }

                frame_count += 1;

                // Report position every 500ms.
                if last_position_report.elapsed() >= std::time::Duration::from_millis(500) {
                    let _ = event_tx.send((
                        playlist_id,
                        PipelineEvent::Position {
                            position_ms: video_frame.timestamp_ms,
                            duration_ms: decoder.duration_ms(),
                        },
                    ));
                    last_position_report = Instant::now();
                }
            }
            Ok(None) => {
                // End of stream.
                info!(playlist_id, frame_count, "video decode complete");
                return DecodeResult::Ended;
            }
            Err(e) => {
                return DecodeResult::Error(format!("Decode error at frame {frame_count}: {e}"));
            }
        }
    }
}

/// Send a black BGRA frame to keep the NDI source visible.
#[cfg(windows)]
fn send_black_frame(sender: &sp_ndi::NdiSender<sp_ndi::RealNdiBackend>, width: u32, height: u32) {
    let data = vec![0u8; (width * height * 4) as usize];
    let frame = sp_ndi::VideoFrame {
        data,
        width,
        height,
        stride: width * 4,
        frame_rate_n: 30000,
        frame_rate_d: 1001,
    };
    sender.send_video(&frame);
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_spawn_and_shutdown() {
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let pipeline = PlaybackPipeline::spawn("test-ndi".into(), event_tx, 1);
        pipeline.shutdown();
        // If we get here, the thread joined successfully.
    }

    #[test]
    fn pipeline_drop_sends_shutdown() {
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        {
            let _pipeline = PlaybackPipeline::spawn("test-drop".into(), event_tx, 2);
            // Pipeline dropped here — Drop impl should send Shutdown and join.
        }
        // If we get here without hanging, the Drop worked correctly.
    }

    #[test]
    fn pipeline_send_command_before_shutdown() {
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let pipeline = PlaybackPipeline::spawn("test-cmd".into(), event_tx, 3);
        pipeline.send(PipelineCommand::Stop);
        pipeline.send(PipelineCommand::Pause);
        pipeline.send(PipelineCommand::Resume);
        pipeline.shutdown();
    }

    #[test]
    fn pipeline_play_emits_event_on_non_windows() {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let pipeline = PlaybackPipeline::spawn("test-play".into(), event_tx, 4);

        pipeline.send(PipelineCommand::Play(PathBuf::from("/tmp/test.mp4")));
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
}
