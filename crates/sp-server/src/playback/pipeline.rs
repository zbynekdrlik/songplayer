//! A playlist's decode pipeline on a dedicated OS thread.
//!
//! [`PlaybackPipeline`] owns a background thread that receives
//! [`PipelineCommand`]s over a crossbeam channel and emits
//! [`PipelineEvent`]s back to the async engine via a Tokio mpsc channel.
//!
//! On Windows the thread decodes the song (`pipeline_paced.rs`: a decode
//! producer over `sp_decoder::SplitSyncedDecoder`) and paces it on the genlock
//! grid (#147) into the pipeline's paced output, which delivers every boundary
//! to the program bus (#209, `paced_output.rs`). #221 lane 3: a playlist has no
//! NDI output of its own — SongPlayer broadcasts only `SP-program` and
//! `SP-program-MAX`, both fed by the bus. On other platforms the thread reports
//! an error for every Play (video decode requires Media Foundation).

use crossbeam_channel::Sender;
use std::thread;
// #196: PathBuf is referenced only by the cfg(windows) DecodeResult::NewPlay
// AND by the `#[path]`-included test module (via `super::*`). Gate the import
// to `any(windows, test)` so it is present in both, but not in the Linux
// non-test lib build where it would be unused.
#[cfg(any(windows, test))]
use std::path::PathBuf;

#[cfg(windows)]
use crate::playback::paced_output::{InstalledBus, PipelineOutput};
#[cfg(windows)]
use crossbeam_channel::{Receiver, TryRecvError};
#[cfg(windows)]
use tracing::{debug, error, info};

// #196: the PipelineCommand / PipelineEvent enums live in a sibling module to
// keep this file small; re-exported so every existing
// `pipeline::PipelineCommand` / `pipeline::PipelineEvent` path still resolves.
#[path = "pipeline_types.rs"]
mod pipeline_types;
pub use pipeline_types::{PipelineCommand, PipelineEvent, real_start_ms};

/// Handle to a playlist's background decode pipeline thread.
pub struct PlaybackPipeline {
    cmd_tx: Sender<PipelineCommand>,
    handle: Option<thread::JoinHandle<()>>,
    output_name: String,
}

impl PlaybackPipeline {
    /// Spawn the pipeline loop on a dedicated OS thread.
    ///
    /// * `output_name` — the playlist's output name (its `ndi_output_name`,
    ///   the scene catalog's identity), the health snapshot's label.
    /// * `event_tx` — channel for sending events back to the async engine.
    /// * `playlist_id` — tags the events with the playlist they belong to.
    /// * `taps` — the dashboard preview taps (#15/#178), offered by the decode
    ///   producer.
    // mutants::skip — Default::default() body-replacement is unsound (no
    // Default impl); the Windows variant is not exercised on the Linux runner.
    // Verified by spawn_stores_the_output_name_for_the_accessor.
    #[cfg(windows)]
    #[cfg_attr(test, mutants::skip)]
    pub fn spawn(
        output_name: String,
        event_tx: tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
        playlist_id: i64,
        taps: crate::playback::preview::preview_stream::DecodeTaps,
    ) -> Self {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();

        let name = output_name.clone();
        let handle = thread::Builder::new()
            .name(format!("pipeline-{playlist_id}"))
            .spawn(move || {
                run_loop(cmd_rx, &name, event_tx, playlist_id, taps);
            })
            .expect("failed to spawn pipeline thread");

        Self {
            cmd_tx,
            handle: Some(handle),
            output_name,
        }
    }

    /// Spawn the pipeline loop on a dedicated OS thread (non-Windows stub).
    // mutants::skip — Default::default() body-replacement is unsound (no Default impl).
    // Correctness verified by spawn_stores_the_output_name_for_the_accessor.
    #[cfg(not(windows))]
    #[cfg_attr(test, mutants::skip)]
    pub fn spawn(
        output_name: String,
        event_tx: tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
        playlist_id: i64,
        // #15 part 2: the decode loop is a stub on non-Windows (no frames are
        // decoded), so the preview taps are never offered to here.
        _taps: crate::playback::preview::preview_stream::DecodeTaps,
    ) -> Self {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();

        let name = output_name.clone();
        let handle = thread::Builder::new()
            .name(format!("pipeline-{playlist_id}"))
            .spawn(move || {
                crate::playback::pipeline_stub::run_loop(cmd_rx, &name, event_tx, playlist_id);
            })
            .expect("failed to spawn pipeline thread");

        Self {
            cmd_tx,
            handle: Some(handle),
            output_name,
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

    /// The output name this pipeline was spawned with (the playlist's
    /// `ndi_output_name`): `playback::ndi_health` labels its snapshots with it.
    pub fn output_name(&self) -> &str {
        &self.output_name
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

/// Main loop for the pipeline thread (Windows).
// mutants::skip — cfg(windows)-only delegation (log + call run_loop_windows);
// dead on the Linux mutation runner, so a body-replacement mutant can never be
// killed by a Linux unit test (same as run_loop_windows).
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
fn run_loop(
    cmd_rx: Receiver<PipelineCommand>,
    output_name: &str,
    event_tx: tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
    taps: crate::playback::preview::preview_stream::DecodeTaps,
) {
    info!(output_name, playlist_id, "pipeline thread started");
    run_loop_windows(cmd_rx, event_tx, playlist_id, taps);
    info!(playlist_id, "pipeline thread exited");
}

/// Windows decode → paced output loop.
///
/// cargo-mutants: skip — this function drives the real MF decode on a real
/// clock and thread. On the Linux mutation runner it is not compiled, so
/// mutations survive with no observable effect. Its decisions live in the
/// cross-platform, Linux-tested `Pacer` and `paced_output.rs`.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
fn run_loop_windows(
    cmd_rx: Receiver<PipelineCommand>,
    event_tx: tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
    taps: crate::playback::preview::preview_stream::DecodeTaps,
) {
    // 1 ms system timer for the boundary-paced sleep granularity (#147).
    crate::playback::pipeline_paced::request_high_res_timer();

    // #221 lane 3: the paced output delivers every boundary to the program bus
    // and nowhere else. Its consumer thread is spawned by the first scope and
    // stopped + joined when this output drops, at the end of this function.
    let mut output = PipelineOutput::new(playlist_id, InstalledBus);
    // The paced scheduler persists across songs (counters accumulate) and
    // re-anchors per Play/Seek.
    let mut pacer = crate::playback::pacer::Pacer::new(sp_core::genlock::GENLOCK_GRID_FPS, true);

    let mut paused = false;
    let mut last_heartbeat = std::time::Instant::now();
    let mut consecutive_bad_polls: u32 = 0;

    loop {
        // No wait: standby is the paced grid's job, so the idle fill starts on
        // the very next boundary after the start, a song end or a stop (#147).
        match cmd_rx.try_recv() {
            Err(TryRecvError::Empty) => {
                crate::playback::pipeline_paced_idle::run_idle_wait(
                    &mut output,
                    &mut pacer,
                    &cmd_rx,
                    &event_tx,
                    playlist_id,
                    paused,
                    &mut last_heartbeat,
                    &mut consecutive_bad_polls,
                );
                continue;
            }
            Err(TryRecvError::Disconnected) => {
                info!(playlist_id, "pipeline thread shutting down (cmd_rx closed)");
                break;
            }
            Ok(PipelineCommand::Shutdown) => {
                info!(playlist_id, "pipeline thread shutting down");
                break;
            }

            Ok(PipelineCommand::Play {
                video,
                audio,
                start_position_ms,
            }) => {
                info!(
                    playlist_id,
                    prev_paused = paused,
                    ?video,
                    ?audio,
                    start_position_ms,
                    "pipeline: Play received (paused -> false)"
                );
                // Inner loop: decode current song; on NewPlay, restart decode
                // with the new pair. Breaks out to outer loop on Ended/Stopped/
                // Error; returns true on Shutdown.
                let mut current_video = video;
                let mut current_audio = audio;
                // `start_position_ms` only applies to the first song of this
                // Play; a NewPlay brings its own.
                let mut current_start_ms = start_position_ms;
                let shutdown_requested = loop {
                    info!(
                        ?current_video,
                        ?current_audio,
                        playlist_id,
                        "starting playback"
                    );
                    paused = false;

                    let decode_result = crate::playback::pipeline_paced::decode_and_send_paced(
                        &cmd_rx,
                        &mut output,
                        &mut pacer,
                        &current_video,
                        &current_audio,
                        &event_tx,
                        playlist_id,
                        &mut paused,
                        &mut last_heartbeat,
                        &mut consecutive_bad_polls,
                        current_start_ms,
                        &taps,
                    );
                    match decode_result {
                        DecodeResult::Ended => {
                            paused = false;
                            info!(playlist_id, "video ended naturally");
                            let _ = event_tx.send((playlist_id, PipelineEvent::Ended));
                            break false;
                        }
                        DecodeResult::Stopped => {
                            paused = false;
                            info!(playlist_id, "playback stopped");
                            break false;
                        }
                        DecodeResult::Shutdown => {
                            info!(playlist_id, "shutdown during playback");
                            break true;
                        }
                        DecodeResult::NewPlay {
                            video: new_v,
                            audio: new_a,
                            start_position_ms: new_start_ms,
                        } => {
                            info!(?new_v, ?new_a, playlist_id, "switching to new song");
                            current_video = new_v;
                            current_audio = new_a;
                            current_start_ms = new_start_ms;
                            continue;
                        }
                        DecodeResult::Error(msg) => {
                            paused = false;
                            error!(playlist_id, %msg, "decode error");
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
                // Seek is a no-op when no song is loaded (the decoder recovers
                // on the next Play).
                debug!(position_ms, "pipeline: seek ignored (no song loaded)");
            }
            Ok(PipelineCommand::Stop) => {
                // Nothing is playing: the idle fill already carries the
                // standby pair on every boundary.
                debug!(playlist_id, "stopped (no active playback)");
            }
        }
    }
}

/// Result of the inner decode loop.
#[cfg(windows)]
pub(crate) enum DecodeResult {
    /// Video reached end of stream.
    Ended,
    /// Stop command received.
    Stopped,
    /// Shutdown command received — thread should exit.
    Shutdown,
    /// A new Play command arrived mid-playback.
    NewPlay {
        video: PathBuf,
        audio: PathBuf,
        start_position_ms: Option<u64>,
    },
    /// Decoder error.
    Error(String),
}

// ---------------------------------------------------------------------------
// Pure helpers (Linux-testable)
// ---------------------------------------------------------------------------

/// Pure predicate: should the pipeline thread run a heartbeat now?
/// Extracted so the timing rule is unit-testable without a live decode loop.
#[cfg(any(windows, test))]
pub(crate) fn should_run_heartbeat(elapsed: std::time::Duration) -> bool {
    elapsed >= std::time::Duration::from_secs(5)
}

/// Pure predicate: is the just-completed poll a "bad poll" per the spec?
/// Used by the paced heartbeat to bump or reset `consecutive_bad_polls`.
/// Branches (state guard, fps, staleness) are individually covered by
/// `heartbeat_decision_tests::classify_bad_poll_*` so the mutation runner
/// can validate every boundary. A bad poll is an underrun or a delivery stale
/// > 10 s.
#[cfg(any(windows, test))]
pub(crate) fn classify_bad_poll(
    state: &crate::playback::ndi_health::PlaybackStateLabel,
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
    // Guard `nominal_fps > 0.0` removed: with nominal=0, observed < 0 is
    // unreachable for non-negative observed, so it was a structurally unkillable mutant.
    if observed_fps < nominal_fps / 2.0 {
        return true;
    }
    if let Some(ts) = last_submit_ts {
        if now.duration_since(ts) > std::time::Duration::from_secs(10) {
            return true;
        }
    }
    false
}

#[cfg(test)]
#[path = "pipeline_inline_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "pipeline_heartbeat_tests.rs"]
mod heartbeat_decision_tests;

#[cfg(test)]
#[path = "pipeline_spawn_tests.rs"]
mod pipeline_spawn_tests;

#[cfg(test)]
#[path = "pipeline_tests_no_sender.rs"]
mod tests_no_sender;
