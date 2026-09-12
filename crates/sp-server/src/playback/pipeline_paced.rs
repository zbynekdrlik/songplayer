//! Windows-only boundary-paced decode driver (#147).
//!
//! The `genlock_pacing`-ON counterpart to `pipeline::decode_and_send`. Instead
//! of leaning on the NDI SDK's `clock_video` cadence, it drives the pure
//! [`Pacer`](crate::playback::pacer::Pacer): sleep-until-boundary on the wall
//! clock, decode forward per the presentation rule, and emit exactly one video
//! frame per grid boundary stamped with the floored boundary wall time. The MF
//! decode + real NDI submit are Windows-only; the scheduling DECISIONS all live
//! in the (cross-platform, Linux-tested) `Pacer`.

use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, TryRecvError};
use tracing::{debug, error, info, warn};

use crate::playback::ndi_health::PlaybackStateLabel;
use crate::playback::pacer::{PacedFrame, Pacer, ServiceOutcome, Standby, plan_sleep_100ns};
use crate::playback::pipeline::{
    DecodeResult, PipelineCommand, PipelineEvent, emit_heartbeat, should_run_heartbeat,
};
use crate::playback::submitter::FrameSubmitter;

/// Request a 1 ms Windows multimedia timer so the paced sleep granularity is
/// ~1 ms rather than the default ~15.6 ms. Called once per paced pipeline
/// thread. `timeBeginPeriod`/`timeEndPeriod` are ref-counted; the matching
/// `timeEndPeriod` is intentionally omitted (the pipeline thread lives for the
/// process lifetime). winmm is always present on Windows.
pub(crate) fn request_high_res_timer() {
    #[link(name = "winmm")]
    unsafe extern "system" {
        fn timeBeginPeriod(u_period: u32) -> u32;
    }
    // SAFETY: winmm.timeBeginPeriod is a leaf call with no invariants beyond a
    // valid period; 1 ms is in range.
    unsafe {
        timeBeginPeriod(1);
    }
    info!("requested 1 ms system timer (timeBeginPeriod) for genlock pacing");
}

/// Convert an MF-decoded video frame + its audio chunks into a [`PacedFrame`].
/// `pts_offset_ms` is subtracted so the PTS is measured from playback start
/// (0-based) — the origin the pacer maps onto the wall grid.
fn to_paced_frame(
    video: sp_decoder::DecodedVideoFrame,
    audio: Vec<sp_decoder::DecodedAudioFrame>,
    pts_offset_ms: u64,
) -> PacedFrame {
    let rel_ms = video.timestamp_ms.saturating_sub(pts_offset_ms);
    let pts_ns = (rel_ms as i64).saturating_mul(1_000_000);
    let ndi_audio: Vec<sp_ndi::AudioFrame> = audio
        .into_iter()
        .map(|af| sp_ndi::AudioFrame {
            data: af.data,
            channels: af.channels,
            sample_rate: af.sample_rate,
            // Stamped by the pacer at submission (raw wall clock, §6).
            timecode_100ns: None,
        })
        .collect();
    PacedFrame {
        pts_ns,
        width: video.width,
        height: video.height,
        stride: video.stride,
        video: video.data,
        audio: ndi_audio,
    }
}

/// Sleep the monotonic clock until ~2 ms before `until_100ns` (wall-clock
/// 100 ns), then spin to the boundary. The coarse wait is clamped to 1 s and a
/// backward clock jump escapes without spinning (#147 change 4, via the pure
/// [`plan_sleep_100ns`]), so a clock step never parks the send thread.
pub(crate) fn sleep_to_boundary(pacer: &Pacer, until_100ns: i64) {
    let interval = pacer.interval_100ns();
    const SPIN_MARGIN_100NS: i64 = 20_000; // ~2 ms
    let plan = plan_sleep_100ns(pacer.now_100ns(), until_100ns, interval);
    if plan.relatch {
        // Clock stepped backward — do not spin; return so the loop re-latches.
        return;
    }
    if plan.sleep_100ns > SPIN_MARGIN_100NS {
        let coarse_100ns = plan.sleep_100ns - SPIN_MARGIN_100NS;
        std::thread::sleep(Duration::from_nanos((coarse_100ns * 100) as u64));
    }
    // Spin the last ~2 ms to hit the boundary precisely; bail on a backward jump
    // (delta grows past one interval) so a clock step never spins forever.
    loop {
        let delta = until_100ns - pacer.now_100ns();
        if delta <= 0 {
            break;
        }
        if interval > 0 && delta > interval {
            break;
        }
        std::hint::spin_loop();
    }
}

/// Boundary-paced inner decode loop. Same command / event contract as
/// `pipeline::decode_and_send`, but the cadence is the wall-clock grid.
#[cfg_attr(test, mutants::skip)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_and_send_paced(
    cmd_rx: &Receiver<PipelineCommand>,
    submitter: &mut FrameSubmitter<sp_ndi::RealNdiBackend>,
    pacer: &mut Pacer,
    video_path: &std::path::Path,
    audio_path: &std::path::Path,
    event_tx: &tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
    paused: &mut bool,
    last_heartbeat: &mut Instant,
    consecutive_bad_polls: &mut u32,
    start_position_ms: Option<u64>,
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

    // Genlock path: NO `set_frame_rate` — emission is on the fixed integer grid
    // the submitter already carries (GENLOCK_GRID_FPS/1, camera-box#1294 §2/§3).

    // PTS origin: the position we start/seek from, so decoded PTS is 0-based.
    let mut pts_offset_ms = start_position_ms.unwrap_or(0);
    if let Some(ms) = start_position_ms {
        if let Err(e) = decoder.seek(ms) {
            warn!(
                playlist_id,
                start_position_ms = ms,
                ?e,
                "paced: seek to start_position_ms failed — playing from 0"
            );
            pts_offset_ms = 0;
        } else {
            info!(
                playlist_id,
                start_position_ms = ms,
                "paced: seeked to start"
            );
        }
    }

    let duration_ms = decoder.duration_ms();
    let _ = event_tx.send((playlist_id, PipelineEvent::Started { duration_ms }));

    // Anchor the wall grid at the first boundary after now (play/seek origin).
    pacer.anchor();

    let mut eos = false;
    let mut last_decoded_ms: u64 = pts_offset_ms;
    let mut last_position_report = Instant::now();

    loop {
        // 1. Commands between boundaries (non-blocking).
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
                debug!(playlist_id, "paced: paused");
            }
            Ok(PipelineCommand::Resume) => {
                *paused = false;
                debug!(playlist_id, "paced: resumed");
            }
            Ok(PipelineCommand::Seek { position_ms }) => match decoder.seek(position_ms) {
                Ok(()) => {
                    pts_offset_ms = position_ms;
                    last_decoded_ms = position_ms;
                    eos = false;
                    // Re-anchor the grid to the seek instant.
                    pacer.anchor();
                }
                Err(e) => {
                    warn!(?e, position_ms, "paced: seek failed");
                }
            },
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                submitter.flush();
                return DecodeResult::Shutdown;
            }
        }

        if *paused {
            // Fill EVERY grid boundary with the frozen last real frame so the
            // receiver stays `locked=` across a pause instead of dropping into
            // holes/underruns — one on-grid stamped frame per boundary via the
            // same Pacer sleep/emit machinery, no audio (#147 fix-lane-2,
            // change 2). Commands are serviced at the loop top every iteration.
            match pacer.service_standby(Standby::FrozenLast, submitter) {
                ServiceOutcome::Wait { until_100ns } => sleep_to_boundary(pacer, until_100ns),
                _ => pacer.tick_wall(),
            }
            if should_run_heartbeat(last_heartbeat.elapsed()) {
                emit_heartbeat(
                    submitter,
                    event_tx,
                    playlist_id,
                    PlaybackStateLabel::Paused,
                    last_heartbeat,
                    consecutive_bad_polls,
                    // Report the flag + accumulated counters, not default (#147
                    // change 7): a paced pipeline is `enabled=true` while paused.
                    pacer.stats(),
                );
            }
            continue;
        }

        // 2. One pacer scheduling step. The pacer reads its own wall clock (a
        //    scheduling read, then an emit read after decode). The pull closure
        //    decodes forward; the submitter is the sink (audio-before-video
        //    async NDI submit).
        let outcome = pacer.service(
            || {
                if eos {
                    return None;
                }
                match decoder.next_synced() {
                    Ok(Some((video_frame, audio_frames))) => {
                        last_decoded_ms = video_frame.timestamp_ms;
                        Some(to_paced_frame(video_frame, audio_frames, pts_offset_ms))
                    }
                    Ok(None) => {
                        eos = true;
                        None
                    }
                    Err(e) => {
                        error!(playlist_id, %e, "paced: decode error");
                        eos = true;
                        None
                    }
                }
            },
            submitter,
        );

        match outcome {
            ServiceOutcome::Wait { until_100ns } => {
                sleep_to_boundary(pacer, until_100ns);
            }
            ServiceOutcome::Emitted | ServiceOutcome::Repeated | ServiceOutcome::Starved => {
                pacer.tick_wall();

                if should_run_heartbeat(last_heartbeat.elapsed()) {
                    emit_heartbeat(
                        submitter,
                        event_tx,
                        playlist_id,
                        PlaybackStateLabel::Playing,
                        last_heartbeat,
                        consecutive_bad_polls,
                        pacer.stats(),
                    );
                }

                if last_position_report.elapsed() >= Duration::from_millis(500) {
                    let _ = event_tx.send((
                        playlist_id,
                        PipelineEvent::Position {
                            position_ms: last_decoded_ms,
                            duration_ms,
                        },
                    ));
                    last_position_report = Instant::now();
                }

                // End when the decoder is exhausted and nothing is parked — the
                // last frame has been shown; do not repeat past end of stream.
                if eos && !pacer.has_pending() {
                    info!(playlist_id, "paced: video decode complete");
                    submitter.flush();
                    return DecodeResult::Ended;
                }
            }
        }
    }
}
