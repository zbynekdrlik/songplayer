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

use crossbeam_channel::Sender;
use std::path::PathBuf;
use std::thread;

// Used in cfg(windows) blocks:
// FrameSubmitter is also needed under `test` cfg — emit_heartbeat /
// run_heartbeat_paused are generic over `B: NdiBackend` so they can be
// unit-tested on Linux CI via MockNdiBackend (see pipeline_heartbeat_tests.rs
// / #133); the live decode loop still only ever instantiates them with the
// real (Windows-only) NDI backend.
#[cfg(any(windows, test))]
use crate::playback::submitter::FrameSubmitter;
#[cfg(windows)]
use crossbeam_channel::{Receiver, TryRecvError};
#[cfg(windows)]
use std::time::Instant;
#[cfg(windows)]
use tracing::{debug, error, info, warn};

// #196: the PipelineCommand / PipelineEvent enums live in a sibling module to
// keep this file under the 1000-line cap; re-exported so every existing
// `pipeline::PipelineCommand` / `pipeline::PipelineEvent` path still resolves.
#[path = "pipeline_types.rs"]
mod pipeline_types;
pub use pipeline_types::{PipelineCommand, PipelineEvent};

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
    // mutants::skip — Default::default() body-replacement is unsound (no Default impl);
    // Windows variant not exercised on Linux runner. Verified by spawn_stores_ndi_name_for_accessor.
    #[cfg(windows)]
    #[cfg_attr(test, mutants::skip)]
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        ndi_name: String,
        ndi_backend: Option<SharedNdiBackend>,
        event_tx: tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
        playlist_id: i64,
        genlock_pacing: bool,
        burn_on: std::sync::Arc<std::sync::atomic::AtomicBool>,
        taps: crate::playback::preview::preview_stream::DecodeTaps,
        // #196: fired with the sender's advertised URL the moment the NDI sender
        // is created, so `create_startup_senders` can serialize creation in id
        // order for a stable name→port map.
        ready_tx: Option<tokio::sync::oneshot::Sender<Option<String>>>,
    ) -> Self {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();

        let ndi_name_for_self = ndi_name.clone();
        let handle = thread::Builder::new()
            .name(format!("pipeline-{playlist_id}"))
            .spawn(move || {
                run_loop(
                    cmd_rx,
                    &ndi_name,
                    ndi_backend,
                    event_tx,
                    playlist_id,
                    genlock_pacing,
                    burn_on,
                    taps,
                    ready_tx,
                );
            })
            .expect("failed to spawn pipeline thread");

        Self {
            cmd_tx,
            handle: Some(handle),
            ndi_name: ndi_name_for_self,
        }
    }

    /// Spawn the decode-to-NDI loop on a dedicated OS thread (non-Windows stub).
    // mutants::skip — Default::default() body-replacement is unsound (no Default impl).
    // Correctness verified by spawn_stores_ndi_name_for_accessor.
    #[cfg(not(windows))]
    #[cfg_attr(test, mutants::skip)]
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        ndi_name: String,
        _ndi_backend: Option<()>,
        event_tx: tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
        playlist_id: i64,
        _genlock_pacing: bool,
        _burn_on: std::sync::Arc<std::sync::atomic::AtomicBool>,
        // #15 part 2: the decode loop is a stub on non-Windows (no frames are
        // decoded), so the preview tap is never offered to here.
        _taps: crate::playback::preview::preview_stream::DecodeTaps,
        // #196: no NDI sender exists on this platform — report "ready, no URL"
        // at once so a startup serializer never blocks on the stub.
        ready_tx: Option<tokio::sync::oneshot::Sender<Option<String>>>,
    ) -> Self {
        if let Some(tx) = ready_tx {
            let _ = tx.send(None);
        }
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();

        let ndi_name_for_self = ndi_name.clone();
        let handle = thread::Builder::new()
            .name(format!("pipeline-{playlist_id}"))
            .spawn(move || {
                crate::playback::pipeline_stub::run_loop(cmd_rx, &ndi_name, event_tx, playlist_id);
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

/// Main loop for the pipeline thread (Windows).
#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn run_loop(
    cmd_rx: Receiver<PipelineCommand>,
    ndi_name: &str,
    ndi_backend: Option<SharedNdiBackend>,
    event_tx: tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
    genlock_pacing: bool,
    burn_on: std::sync::Arc<std::sync::atomic::AtomicBool>,
    taps: crate::playback::preview::preview_stream::DecodeTaps,
    ready_tx: Option<tokio::sync::oneshot::Sender<Option<String>>>,
) {
    info!(
        ndi_name,
        playlist_id, genlock_pacing, "pipeline thread started"
    );
    run_loop_windows(
        cmd_rx,
        ndi_name,
        ndi_backend,
        event_tx,
        playlist_id,
        genlock_pacing,
        burn_on,
        taps,
        ready_tx,
    );
    info!(playlist_id, "pipeline thread exited");
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
#[allow(clippy::too_many_arguments)]
fn run_loop_windows(
    cmd_rx: Receiver<PipelineCommand>,
    ndi_name: &str,
    ndi_backend: Option<SharedNdiBackend>,
    event_tx: tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
    genlock_pacing: bool,
    burn_on: std::sync::Arc<std::sync::atomic::AtomicBool>,
    taps: crate::playback::preview::preview_stream::DecodeTaps,
    mut ready_tx: Option<tokio::sync::oneshot::Sender<Option<String>>>,
) {
    // 1 ms system timer for the boundary-paced sleep granularity (#147).
    if genlock_pacing {
        crate::playback::pipeline_paced::request_high_res_timer();
    }

    let backend = match ndi_backend {
        Some(b) => b,
        None => {
            error!("no NDI backend provided");
            // #196: unblock the startup serializer — this output has no sender.
            if let Some(tx) = ready_tx.take() {
                let _ = tx.send(None);
            }
            let _ = event_tx.send((
                playlist_id,
                PipelineEvent::Error("NDI SDK not available".into()),
            ));
            wait_for_shutdown(&cmd_rx, playlist_id);
            return;
        }
    };

    // Genlock (#147): ON = the app owns the cadence (clock_video=false, paced on
    // the wall-clock grid); OFF = today's SDK-clocked path (clock_video=true).
    // clock_audio stays false either way (single submission thread).
    let sender_result = if genlock_pacing {
        sp_ndi::NdiSender::new_with_clocking(backend, ndi_name, false, false)
    } else {
        sp_ndi::NdiSender::new_with_clocking(backend, ndi_name, true, false)
    };
    let sender = match sender_result {
        Ok(s) => s,
        Err(e) => {
            error!(%e, "failed to create NDI sender");
            // #196: unblock the startup serializer — creation failed.
            if let Some(tx) = ready_tx.take() {
                let _ = tx.send(None);
            }
            let _ = event_tx.send((
                playlist_id,
                PipelineEvent::Error(format!("Failed to create NDI sender: {e}")),
            ));
            wait_for_shutdown(&cmd_rx, playlist_id);
            return;
        }
    };

    // #196: read the advertised source URL now (right after create, before the
    // sender is moved into the submitter) so the startup log carries the
    // name→port map and the serializer can proceed to the next output in order.
    let sender_url = sender.source_url();
    info!(
        ndi_name,
        url = sender_url.as_deref().unwrap_or("unknown"),
        genlock_pacing,
        "ndi: sender ready"
    );
    if let Some(tx) = ready_tx.take() {
        let _ = tx.send(sender_url);
    }

    // Initial black frame. Genlock path emits on the fixed integer grid
    // (GENLOCK_GRID_FPS/1) and skips the per-file `set_frame_rate`; the legacy
    // path updates it per-file in `decode_and_send`.
    let mut submitter = FrameSubmitter::new(sender, sp_core::genlock::GENLOCK_GRID_FPS as i32, 1);
    submitter.set_paced(genlock_pacing); // paced: stamp standby frames on-grid (#147)
    // #151: install the shared burn flag so the runtime API toggle drives the
    // paced-emit overlay. Default OFF; only the paced path ever paints.
    submitter.set_burn_flag(burn_on);
    submitter.send_black_bgra(1920, 1080);

    // #192: on the SDK-clocked path a dedicated wall-clock audio emitter thread
    // clocks the NDI audio stream continuously (silence-filled across song
    // transitions / heavy-child stalls). Declared AFTER `submitter` so its guard
    // drops (shutdown + JOIN) BEFORE the sender is destroyed — `send_destroy`
    // invalidates the handle the emitter's AudioSink holds. Paced path keeps its
    // own audio clock (the Pacer's AudioGridBuffer + PLL), so no emitter there.
    let audio_emitter = if genlock_pacing {
        None
    } else {
        // spawn returns None on OS-thread-spawn failure → decode_and_send takes
        // the legacy audio-with-video path (never a hung, undrained ring).
        let shared = crate::playback::pipeline::audio_emitter::new_shared_emitter();
        crate::playback::pipeline::pipeline_audio::spawn_audio_emitter(
            ndi_name,
            submitter.audio_sink(),
            shared,
        )
    };

    // The paced scheduler persists across songs (counters accumulate) and
    // re-anchors per Play/Seek. Disabled + unused on the legacy path.
    let mut pacer =
        crate::playback::pacer::Pacer::new(sp_core::genlock::GENLOCK_GRID_FPS, genlock_pacing);

    let mut paused = false;
    let mut last_heartbeat = std::time::Instant::now();
    let mut consecutive_bad_polls: u32 = 0;

    loop {
        match cmd_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                // Idle wait: paced ON fills every grid boundary with black while
                // no song is loaded; OFF keeps the plain 5 s heartbeat. All the
                // logic lives in the sibling module (#147 fix-lane-2).
                crate::playback::pipeline_paced_idle::run_idle_wait(
                    genlock_pacing,
                    &mut submitter,
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
                // `start_position_ms` only applies to the first decode_and_send
                // call; subsequent calls triggered by NewPlay start from 0.
                let mut current_start_ms = start_position_ms;
                let shutdown_requested = loop {
                    info!(
                        ?current_video,
                        ?current_audio,
                        playlist_id,
                        "starting playback"
                    );
                    paused = false;

                    let decode_result = if genlock_pacing {
                        crate::playback::pipeline_paced::decode_and_send_paced(
                            &cmd_rx,
                            &mut submitter,
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
                        )
                    } else {
                        decode_and_send(
                            &cmd_rx,
                            &mut submitter,
                            &current_video,
                            &current_audio,
                            &event_tx,
                            playlist_id,
                            &mut paused,
                            &mut last_heartbeat,
                            &mut consecutive_bad_polls,
                            current_start_ms,
                            &taps,
                            audio_emitter.as_ref().map(|t| t.shared()),
                        )
                    };
                    match decode_result {
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

/// Inner decode loop: open both sidecar files, read synced frames, send to NDI.
///
/// Returns when the video ends or a Stop/Shutdown/Play command is received.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
#[allow(clippy::too_many_arguments)]
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
    start_position_ms: Option<u64>,
    taps: &crate::playback::preview::preview_stream::DecodeTaps,
    audio_emitter: Option<&crate::playback::pipeline::audio_emitter::SharedEmitter>,
) -> DecodeResult {
    use sp_decoder::MediaFoundationVideoReader;

    let video_reader = match MediaFoundationVideoReader::open(video_path) {
        Ok(v) => v,
        Err(e) => {
            return DecodeResult::Error(format!(
                "failed to open video {}: {e}",
                video_path.display()
            ));
        }
    };
    // #14/#186: honour the live karaoke control — a plain mix reader, or a live
    // stem-mixing StemMixReader (with original-mix fallback when stems are missing).
    let audio_stream =
        match crate::stems::reader::open_audio_stream(audio_path, &crate::stems::control::global())
        {
            Ok(a) => a,
            Err(e) => {
                return DecodeResult::Error(format!(
                    "failed to open audio {}: {e}",
                    audio_path.display()
                ));
            }
        };
    let mut decoder = match crate::playback::pipeline::pipeline_audio::open_synced_decoder(
        Box::new(video_reader),
        audio_stream,
        audio_emitter,
    ) {
        Ok(d) => d,
        Err(e) => {
            return DecodeResult::Error(format!("SplitSyncedDecoder::new failed: {e}"));
        }
    };

    // Apply the file's real frame rate to the submitter so NDI paces correctly.
    let (num, den) = decoder.frame_rate();
    submitter.set_frame_rate(num as i32, den as i32);

    // Atomic play-from-position (issue #88): seek BEFORE the frame-submission
    // loop so there is no race window between Play and Seek. Seek failures are
    // non-fatal — the song still plays from 0 with a logged warning so the
    // operator always gets audio/video rather than a silent abort.
    if let Some(ms) = start_position_ms {
        if let Err(e) = decoder.seek(ms) {
            warn!(
                playlist_id,
                start_position_ms = ms,
                ?e,
                "decode_and_send: seek to start_position_ms failed — playing from 0"
            );
        } else {
            info!(
                playlist_id,
                start_position_ms = ms,
                "decode_and_send: seeked to start position"
            );
        }
    }

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
            Ok(PipelineCommand::Play {
                video,
                audio,
                start_position_ms,
            }) => {
                submitter.flush();
                return DecodeResult::NewPlay {
                    video,
                    audio,
                    start_position_ms,
                };
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
                crate::playback::pipeline::pipeline_audio::clear_if_present(audio_emitter);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                submitter.flush();
                return DecodeResult::Shutdown;
            }
        }

        if *paused {
            crate::playback::pipeline::pipeline_audio::hold_if_present(audio_emitter);
            submitter.send_black_bgra(1920, 1080);
            // #133: without this, /api/v1/ndi/health froze on the last
            // pre-pause HealthSnapshot (state=Playing, stale fps) for as
            // long as the pause lasted — this loop never reached the
            // decode-success arm below, which was the only place a
            // heartbeat was emitted while inside decode_and_send.
            // run_heartbeat_paused self-gates on the same 5s cadence as the
            // Playing branch, so calling it every 100ms poll is safe.
            //
            // Known rolling-window edge case: if pause begins mid-window
            // (not exactly on a 5s heartbeat boundary), THIS first paused
            // heartbeat's drain_window() still contains whatever frames
            // were submitted during the Playing portion of that window, so
            // observed_fps briefly reads a blended (nonzero) value. It
            // converges to a true 0.0 on the FOLLOWING heartbeat, once a
            // full window has elapsed with no submissions. Not a
            // regression — the same window-drain semantics the Playing
            // branch already has; harmless because classify_bad_poll never
            // flags a Paused state as a bad poll regardless of fps.
            run_heartbeat_paused(
                submitter,
                event_tx,
                playlist_id,
                last_heartbeat,
                consecutive_bad_polls,
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
            continue;
        }

        match decoder.next_synced() {
            Ok(Some((video_frame, audio_frames))) => {
                // #15/#178: offer video + post-mix audio to BOTH preview taps
                // BEFORE the NDI submit / audio-emitter push consume the frame.
                // No viewer => a couple of relaxed atomic loads; never blocks,
                // never adds latency to the NDI submit / genlock path.
                taps.offer_frame(&video_frame, &audio_frames);
                // #192: on the SDK-clocked path audio is PUSHED into the
                // wall-clock emitter's ring (before this frame's submit) and the
                // TIME_CRITICAL emit thread clocks it out continuously, so a song
                // end / heavy-child stall no longer stops the audio stream;
                // `submit_nv12` then carries video only. (Emitter absent → legacy
                // audio-with-video fallback.) See `pipeline_audio`.
                let ndi_audio = crate::playback::pipeline::pipeline_audio::push_or_collect_audio(
                    audio_emitter,
                    audio_frames,
                );

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
                        audio_emitter,
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
                // #192 item 3: natural end only — let the emit thread drain the
                // ring (≤ 400 ms) before the next song's clear_ring wipes its tail.
                crate::playback::pipeline::pipeline_audio::drain_if_present(audio_emitter);
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
#[cfg(any(windows, test))]
pub(crate) fn should_run_heartbeat(elapsed: std::time::Duration) -> bool {
    elapsed >= std::time::Duration::from_secs(5)
}

/// Pure predicate: is the just-completed poll a "bad poll" per the spec?
/// Used by the pipeline thread to bump or reset `consecutive_bad_polls`.
/// Branches (state guard, connections, fps, staleness) are individually
/// covered by `heartbeat_decision_tests::classify_bad_poll_*` so the
/// mutation runner can validate every boundary.
#[cfg(any(windows, test))]
pub(crate) fn classify_bad_poll(
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

// ---------------------------------------------------------------------------
// Windows heartbeat helpers
// ---------------------------------------------------------------------------

// The idle/no-song heartbeat (`run_heartbeat_outer`) moved into the sibling
// `pipeline_paced_idle::run_idle_wait` (#147 fix-lane-2), which now owns the
// whole idle wait for BOTH flag states — paced fill vs plain heartbeat.

// mutants::skip — Windows-only plumbing for emit_heartbeat (which is also
// skipped); no Linux test path. Always emits with state=Playing.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
fn run_heartbeat_inner(
    submitter: &mut FrameSubmitter<sp_ndi::RealNdiBackend>,
    event_tx: &tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
    last_heartbeat: &mut std::time::Instant,
    consecutive_bad_polls: &mut u32,
    audio_emitter: Option<&crate::playback::pipeline::audio_emitter::SharedEmitter>,
) {
    // #192: surface the wall-clock emitter telemetry under audio.emitter on
    // /api/v1/ndi/health (disabled default without an emitter).
    let audio = crate::playback::pipeline::audio_emitter::heartbeat_audio_stats(audio_emitter);
    emit_heartbeat(
        submitter,
        event_tx,
        playlist_id,
        crate::playback::ndi_health::PlaybackStateLabel::Playing,
        last_heartbeat,
        consecutive_bad_polls,
        // Idle / paused / SDK-clocked heartbeats carry no pacing telemetry; the
        // boundary-paced decode loop passes real `Pacer` stats (#147).
        crate::playback::ndi_health::PacingStats::default(),
        audio,
    );
}

/// #133: sibling of `run_heartbeat_inner` for `decode_and_send`'s paused
/// branch. Always reports `PlaybackStateLabel::Paused` — unlike the idle
/// heartbeat in `pipeline_paced_idle::run_idle_wait` (used only when no song is
/// loaded at all, where Idle-vs-Paused is ambiguous), this call site knows for
/// certain a song is
/// mid-decode and simply not advancing, so there is no Idle case to
/// distinguish. Unlike `run_heartbeat_inner` (whose 5s-cadence gate lives at
/// the call site, `if should_run_heartbeat(...) { run_heartbeat_inner(...) }`),
/// this one self-gates internally so the paused branch can call it
/// unconditionally on every 100ms poll — and so the cadence behaviour is
/// directly unit-testable (see `paused_heartbeat_respects_5s_cadence` in
/// `pipeline_heartbeat_tests.rs`). Generic + `#[cfg(any(windows, test))]` for
/// the same reason as `emit_heartbeat`. Deliberately NOT `mutants::skip` —
/// unlike its siblings, both branches (gated / emits) have direct assertions
/// in `pipeline_heartbeat_tests.rs`, so mutation testing should hold it to
/// account.
#[cfg(any(windows, test))]
fn run_heartbeat_paused<B: sp_ndi::NdiBackend>(
    submitter: &mut FrameSubmitter<B>,
    event_tx: &tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
    last_heartbeat: &mut std::time::Instant,
    consecutive_bad_polls: &mut u32,
) {
    if !should_run_heartbeat(last_heartbeat.elapsed()) {
        return;
    }
    emit_heartbeat(
        submitter,
        event_tx,
        playlist_id,
        crate::playback::ndi_health::PlaybackStateLabel::Paused,
        last_heartbeat,
        consecutive_bad_polls,
        // Idle / paused / SDK-clocked heartbeats carry no pacing telemetry; the
        // boundary-paced decode loop passes real `Pacer` stats (#147).
        crate::playback::ndi_health::PacingStats::default(),
        crate::playback::ndi_health::AudioStats::default(),
    );
}

// #133: generic over `B: NdiBackend` (rather than hardcoded to the
// Windows-only `RealNdiBackend`) and gated `#[cfg(any(windows, test))]`
// (rather than plain `#[cfg(windows)]`) so it — and the paused-heartbeat
// path built on top of it — can be exercised directly on Linux CI via
// MockNdiBackend. FrameSubmitter<B> itself has always been generic (see
// submitter.rs's own MockNdiBackend-based tests); only this function's
// signature was needlessly narrowed. The live pipeline thread still only
// ever instantiates it with the real NDI backend via run_loop_windows /
// decode_and_send, both of which stay `#[cfg(windows)]`-only.
#[cfg(any(windows, test))]
#[cfg_attr(test, mutants::skip)]
#[allow(clippy::too_many_arguments)] // heartbeat carries the pacing + audio gauges (#147/#150); a struct would just move the same 8 fields
pub(crate) fn emit_heartbeat<B: sp_ndi::NdiBackend>(
    submitter: &mut FrameSubmitter<B>,
    event_tx: &tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
    state: crate::playback::ndi_health::PlaybackStateLabel,
    last_heartbeat: &mut std::time::Instant,
    consecutive_bad_polls: &mut u32,
    pacing: crate::playback::ndi_health::PacingStats,
    audio: crate::playback::ndi_health::AudioStats,
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
            pacing,
            audio,
        },
    ));
    *last_heartbeat = now;
}

// #192: wall-clock audio emitter for the SDK-clocked path. The pure core (ring
// + grid + telemetry) is cross-platform and Linux-tested; the Windows-only
// thread lifecycle (TIME_CRITICAL spawn, sleep/spin loop, join-before-drop)
// lives in the `pipeline_audio` sibling. Registered here (not in mod.rs) to
// keep mod.rs off the 1000-line cap.
#[path = "audio_emitter.rs"]
pub mod audio_emitter;

#[cfg(windows)]
#[path = "pipeline_audio.rs"]
pub(crate) mod pipeline_audio;

#[cfg(test)]
#[path = "pipeline_inline_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "pipeline_heartbeat_tests.rs"]
mod heartbeat_decision_tests;

#[cfg(test)]
#[path = "pipeline_spawn_tests.rs"]
mod pipeline_spawn_tests;
