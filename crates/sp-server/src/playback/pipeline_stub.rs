//! Non-Windows pipeline stub — extracted from `pipeline.rs` to keep it under
//! the 1000-line cap. Video decode requires Media Foundation (Windows only),
//! so on other platforms the pipeline thread just waits for commands and
//! reports an error for every `Play`.

use crossbeam_channel::Receiver;
use tracing::{info, warn};

use super::pipeline::{PipelineCommand, PipelineEvent};

/// Main loop for the pipeline thread (non-Windows).
pub(crate) fn run_loop(
    cmd_rx: Receiver<PipelineCommand>,
    ndi_name: &str,
    event_tx: tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
) {
    info!(ndi_name, playlist_id, "pipeline thread started");
    run_loop_stub(cmd_rx, ndi_name, event_tx, playlist_id);
    info!(playlist_id, "pipeline thread exited");
}

/// Non-Windows stub: waits for commands and reports errors for Play.
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
            Ok(PipelineCommand::Play {
                video,
                audio,
                start_position_ms: _,
            }) => {
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
