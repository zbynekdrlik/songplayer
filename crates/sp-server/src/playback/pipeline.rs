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
/// On Windows this would initialise NDI and the decoder, then enter a frame
/// loop.  On non-Windows it simply waits for commands and reports errors for
/// Play commands (since Media Foundation is not available).
fn run_loop(
    cmd_rx: Receiver<PipelineCommand>,
    ndi_name: &str,
    event_tx: tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
) {
    info!(ndi_name, playlist_id, "pipeline thread started");

    loop {
        match cmd_rx.recv() {
            Ok(PipelineCommand::Shutdown) | Err(_) => {
                info!(playlist_id, "pipeline thread shutting down");
                break;
            }

            Ok(PipelineCommand::Play(path)) => {
                info!(?path, playlist_id, "pipeline: Play command received");

                #[cfg(windows)]
                {
                    // Windows: decode via Media Foundation and send via NDI.
                    // This is a placeholder — the actual decode loop will be
                    // implemented when sp-decoder integration is wired up.
                    let _ = event_tx.send((
                        playlist_id,
                        PipelineEvent::Error("Windows decode pipeline not yet wired".into()),
                    ));
                }

                #[cfg(not(windows))]
                {
                    warn!(?path, "video decode not available on this platform");
                    let _ = event_tx.send((
                        playlist_id,
                        PipelineEvent::Error(
                            "Video decode requires Windows (Media Foundation)".into(),
                        ),
                    ));
                }
            }

            Ok(PipelineCommand::Pause) => {
                info!(playlist_id, "pipeline: paused");
            }

            Ok(PipelineCommand::Resume) => {
                info!(playlist_id, "pipeline: resumed");
            }

            Ok(PipelineCommand::Stop) => {
                info!(playlist_id, "pipeline: stopped");
            }
        }
    }

    info!(playlist_id, "pipeline thread exited");
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
