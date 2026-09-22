//! Windows-only boundary-paced decode driver (#147).
//!
//! The `genlock_pacing`-ON counterpart to `pipeline::decode_and_send`. Instead
//! of leaning on the NDI SDK's `clock_video` cadence, it drives the pure
//! [`Pacer`](crate::playback::pacer::Pacer): sleep-until-boundary on the wall
//! clock, decode forward per the presentation rule, and emit exactly one video
//! frame per grid boundary stamped with the floored boundary wall time. The MF
//! decode + real NDI submit are Windows-only; the scheduling DECISIONS all live
//! in the (cross-platform, Linux-tested) `Pacer`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, TryRecvError};
use tracing::{debug, error, info, warn};

use crate::playback::frame_buf::SharedFrame;
use crate::playback::ndi_health::{PacingStats, PlaybackStateLabel};
use crate::playback::pacer::{PacedFrame, Pacer, ServiceOutcome, Standby, plan_sleep_100ns};
use crate::playback::pacer_queue::{ProducerAction, SharedQueue};
use crate::playback::pipeline::{
    DecodeResult, PipelineCommand, PipelineEvent, should_run_heartbeat,
};
use crate::playback::pipeline_paced_submit::{
    HandoffSink, SharedHandoff, StopOnPanic, emit_heartbeat_paced, run_submit_consumer,
};
use crate::playback::submit_handoff::SUBMIT_HANDOFF_BOUND;
use crate::playback::submitter::FrameSubmitter;

/// A frame handed from the decode producer to the emit consumer (#147): the paced
/// frame plus the ABSOLUTE decoded position (ms) for the pipeline's Position
/// events.
type QueuedFrame = (PacedFrame, u64);

/// Look-ahead depth of the decode queue (#147 box test 4 fix). ≥ 8 frames ≈
/// 330 ms at 24 fps (> 3× the observed 93–111 ms decode p99), so a decode-tail
/// spike while the #162 stems child is resident is absorbed by the buffer instead
/// of landing as a late boundary emit. 12 gives comfortable headroom (~500 ms at
/// 24 fps, ~66 MB of NV12 per playing pipeline).
const DECODE_QUEUE_BOUND: usize = 12;

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
        // Wrap the decoded NV12 buffer ONCE on the producer thread (#203 2b);
        // every downstream hop (pacer repeat, handoff, submit holdover) is an
        // `Arc` bump of this allocation, recycled to the frame pool on last drop.
        video: SharedFrame::new(video.data),
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

/// One INFO line summarising a song's paced emission (#147 lane 3, change 4):
/// per-song DELTAS of the accumulating pacer counters (the health doc keeps the
/// cumulative values) plus the per-song lag/iteration gauges. Emitted once at
/// every song-end (EOS / stop / next / shutdown). `iter_p99_us >= interval`
/// (≈ 33_333 µs @30 fps) is the "decoder couldn't keep up" signal that drives
/// `max_lag_slots` up and forces the re-anchor.
fn log_song_summary(
    pacer: &Pacer,
    base: &PacingStats,
    song_start: Instant,
    playlist_id: i64,
    reason: &str,
) {
    let s = pacer.stats();
    let a = pacer.audio_stats();
    info!(
        playlist_id,
        reason,
        emits = s.seq.saturating_sub(base.seq),
        repeats = s.repeats.saturating_sub(base.repeats),
        dropped = s.dropped.saturating_sub(base.dropped),
        resyncs = s.resyncs.saturating_sub(base.resyncs),
        relatches = s.relatches.saturating_sub(base.relatches),
        max_lag_slots = pacer.max_lag_slots(),
        iter_p50_us = pacer.iter_p50_us(),
        iter_p99_us = pacer.iter_p99_us(),
        // Audio clock discipline (#148): the file-clock residual, the applied
        // slow-resample correction, and the cumulative buffer underruns.
        audio_residual_ppm = a.residual_ppm,
        audio_applied_ppm = a.applied_ppm,
        audio_underruns = a.underruns,
        duration_s = song_start.elapsed().as_secs_f32(),
        "paced: song summary"
    );
}

/// The decode PRODUCER thread (#147 producer/consumer split). Owns the ENTIRE
/// MediaFoundation decoder lifecycle on ITS thread (COM STA affinity — the thread
/// that opens the reader must be the one that decodes / seeks / drops it), pulls
/// frames as fast as the bounded queue allows (blocking on backpressure), and
/// pushes them for the emit thread to pop at grid boundaries. Reports the media
/// duration (or an open error) back over `open_tx`. Preview sampling happens HERE
/// — off the time-critical emit/submit path (`preview.md`). Exits on a Stop from
/// the consumer, dropping the decoder on this thread.
#[cfg_attr(test, mutants::skip)]
fn run_decode_producer(
    video_path: std::path::PathBuf,
    audio_path: std::path::PathBuf,
    start_position_ms: Option<u64>,
    shared: Arc<SharedQueue<QueuedFrame>>,
    open_tx: crossbeam_channel::Sender<Result<(u64, f32), String>>,
    taps: crate::playback::preview::preview_stream::DecodeTaps,
    playlist_id: i64,
) {
    use sp_decoder::{MediaFoundationVideoReader, SplitSyncedDecoder};

    let video_reader = match MediaFoundationVideoReader::open(&video_path) {
        Ok(v) => v,
        Err(e) => {
            let _ = open_tx.send(Err(format!(
                "failed to open video {}: {e}",
                video_path.display()
            )));
            return;
        }
    };
    // #14/#186: karaoke-aware audio source (plain mix or live StemMixReader with mix fallback).
    let audio_stream = match crate::stems::reader::open_audio_stream(
        &audio_path,
        &crate::stems::control::global(),
    ) {
        Ok(a) => a,
        Err(e) => {
            let _ = open_tx.send(Err(format!(
                "failed to open audio {}: {e}",
                audio_path.display()
            )));
            return;
        }
    };
    let mut decoder = match SplitSyncedDecoder::new(Box::new(video_reader), audio_stream) {
        Ok(d) => d,
        Err(e) => {
            let _ = open_tx.send(Err(format!("SplitSyncedDecoder::new failed: {e}")));
            return;
        }
    };

    // PTS origin: the position we start/seek from, so decoded PTS is 0-based.
    let mut pts_offset_ms = start_position_ms.unwrap_or(0);
    if let Some(ms) = start_position_ms {
        if let Err(e) = decoder.seek(ms) {
            warn!(
                playlist_id,
                start_position_ms = ms,
                ?e,
                "paced producer: seek to start_position_ms failed — playing from 0"
            );
            pts_offset_ms = 0;
        } else {
            info!(
                playlist_id,
                start_position_ms = ms,
                "paced producer: seeked to start"
            );
        }
    }

    let duration_ms = decoder.duration_ms();
    // #168 r6b: report the DECODER's source fps alongside the duration so the
    // paced heartbeat can carry a path-independent `source_fps` to the lock rule
    // (the paced submitter carries the grid rate, so `submitter.nominal_fps()`
    // there is the grid, never the source). `(num, den)` is the same pair the
    // SDK path reads at `pipeline.rs` before `set_frame_rate`.
    let (num, den) = decoder.frame_rate();
    let source_fps = if den == 0 {
        0.0
    } else {
        num as f32 / den as f32
    };
    if open_tx.send(Ok((duration_ms, source_fps))).is_err() {
        return; // the emit thread is already gone
    }

    let mut epoch: u64 = 0;
    loop {
        match decoder.next_synced() {
            Ok(Some((video_frame, audio_frames))) => {
                // #15/#178: offer video + post-mix audio to BOTH preview taps on
                // the PRODUCER thread — off the time-critical emit/submit path
                // (`preview.md`). No viewer => a couple of relaxed atomic loads.
                taps.offer_frame(&video_frame, &audio_frames);
                let decoded_ms = video_frame.timestamp_ms;
                let item = (
                    to_paced_frame(video_frame, audio_frames, pts_offset_ms),
                    decoded_ms,
                );
                match shared.producer_push(item, epoch) {
                    ProducerAction::Continue => {}
                    ProducerAction::Seek {
                        position_ms,
                        epoch: new_epoch,
                    } => producer_seek(
                        &mut decoder,
                        position_ms,
                        &mut pts_offset_ms,
                        &mut epoch,
                        new_epoch,
                        playlist_id,
                    ),
                    ProducerAction::Stop => return,
                }
            }
            Ok(None) => {
                if producer_drain_wait(
                    &mut decoder,
                    &shared,
                    &mut pts_offset_ms,
                    &mut epoch,
                    playlist_id,
                ) {
                    return;
                }
            }
            Err(e) => {
                error!(playlist_id, %e, "paced producer: decode error");
                if producer_drain_wait(
                    &mut decoder,
                    &shared,
                    &mut pts_offset_ms,
                    &mut epoch,
                    playlist_id,
                ) {
                    return;
                }
            }
        }
    }
}

/// Apply a producer-side seek: seek the decoder, adopt the new PTS origin + epoch.
#[cfg_attr(test, mutants::skip)]
fn producer_seek(
    decoder: &mut sp_decoder::SplitSyncedDecoder,
    position_ms: u64,
    pts_offset_ms: &mut u64,
    epoch: &mut u64,
    new_epoch: u64,
    playlist_id: i64,
) {
    if let Err(err) = decoder.seek(position_ms) {
        warn!(
            playlist_id,
            position_ms,
            ?err,
            "paced producer: seek failed"
        );
    }
    *pts_offset_ms = position_ms;
    *epoch = new_epoch;
}

/// EOS / decode-error: mark end-of-stream, then BLOCK until the consumer requests
/// a seek (scrub after end) or a stop. Returns `true` when the producer should
/// exit its thread. `Continue` is never returned by `wait_after_eos`.
#[cfg_attr(test, mutants::skip)]
fn producer_drain_wait(
    decoder: &mut sp_decoder::SplitSyncedDecoder,
    shared: &Arc<SharedQueue<QueuedFrame>>,
    pts_offset_ms: &mut u64,
    epoch: &mut u64,
    playlist_id: i64,
) -> bool {
    shared.producer_eos();
    match shared.wait_after_eos() {
        ProducerAction::Stop => true,
        ProducerAction::Seek {
            position_ms,
            epoch: new_epoch,
        } => {
            producer_seek(
                decoder,
                position_ms,
                pts_offset_ms,
                epoch,
                new_epoch,
                playlist_id,
            );
            false
        }
        ProducerAction::Continue => false,
    }
}

/// Boundary-paced EMIT loop (#147 producer/consumer split). Same command / event
/// contract as `pipeline::decode_and_send`, but the cadence is the wall-clock grid
/// and the decode happens on a dedicated producer thread ([`run_decode_producer`])
/// that fills a bounded look-ahead queue — box test 4 (2026-09-15) proved a
/// one-frame synchronous look-ahead cannot hold the grid on this box while the
/// #162 stems child is resident. The emit thread only POPS the pre-decoded frame
/// due at the boundary; the `Pacer` decision layer (presentation rule, catch-up,
/// resync, re-anchor, audio grid) is unchanged.
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
    taps: &crate::playback::preview::preview_stream::DecodeTaps,
) -> DecodeResult {
    // Spawn the decode producer — it owns the decoder on its own STA thread and
    // fills the bounded look-ahead queue.
    let shared: Arc<SharedQueue<QueuedFrame>> = Arc::new(SharedQueue::new(DECODE_QUEUE_BOUND));
    let (open_tx, open_rx) = crossbeam_channel::bounded::<Result<(u64, f32), String>>(1);
    let producer = {
        let shared = shared.clone();
        let taps = taps.clone();
        let video_path = video_path.to_path_buf();
        let audio_path = audio_path.to_path_buf();
        std::thread::Builder::new()
            .name(format!("paced-decode-{playlist_id}"))
            .spawn(move || {
                run_decode_producer(
                    video_path,
                    audio_path,
                    start_position_ms,
                    shared,
                    open_tx,
                    taps,
                    playlist_id,
                );
            })
            .expect("spawn paced decode producer thread")
    };

    // Block until the producer has opened the decoder and reported the duration
    // (or an open error). A dead producer (Disconnected) is an open failure.
    let (duration_ms, source_fps) = match open_rx.recv() {
        Ok(Ok(pair)) => pair,
        Ok(Err(msg)) => {
            let _ = producer.join();
            return DecodeResult::Error(msg);
        }
        Err(_) => {
            let _ = producer.join();
            return DecodeResult::Error("paced decode producer exited before open".to_string());
        }
    };
    let _ = event_tx.send((playlist_id, PipelineEvent::Started { duration_ms }));

    // Genlock path: NO `set_frame_rate` — emission is on the fixed integer grid
    // the submitter already carries (GENLOCK_GRID_FPS/1, camera-box#1294 §2/§3).

    // Anchor the wall grid at the first boundary after now (play/seek origin).
    pacer.anchor();

    // Baseline for the per-song summary (#147 lane 3, change 4): the pacer's
    // counters accumulate across songs for the health doc, so the summary
    // reports this song's DELTAS from here.
    let summary_base = pacer.stats();
    let song_start = Instant::now();

    let mut last_decoded_ms: u64 = start_position_ms.unwrap_or(0);
    let mut last_position_report = Instant::now();

    // #168 output-side split: the NDI submit runs on a dedicated thread fed by a
    // BOUNDED handoff, so a `send_video_async` stall never lands as a late
    // boundary emit. The submit thread BORROWS the `FrameSubmitter` for the song
    // via `thread::scope` (SDK per-instance affinity + the async double-buffer
    // holdover stay single-threaded); it joins before this scope returns, so the
    // buffer is flushed before the outer loop reuses the submitter for a black
    // frame. The emit thread emits through a `HandoffSink` (hand off in ~µs) and
    // reads a submit-side snapshot for the heartbeat.
    let handoff = SharedHandoff::new(SUBMIT_HANDOFF_BOUND);
    let handoff_ref = &handoff;
    // Heartbeat window baselines: the honest observed fps is the SUBMIT-side
    // frame count (frames that actually left the box), not the emit count.
    let mut hb_prev_total: u64 = 0;
    let mut hb_prev_instant = Instant::now();

    let result: DecodeResult = std::thread::scope(|s| {
        let sub: &mut FrameSubmitter<sp_ndi::RealNdiBackend> = submitter;
        let submit_join = s.spawn(move || run_submit_consumer(sub, handoff_ref, playlist_id));
        // If the emit loop PANICS, unwind must still stop the submit thread or the
        // scope's join deadlocks on the parked consumer (#168 review 🟡). On the
        // normal path the explicit `stop_with_tail` below wins the tail and this
        // drop is a no-op re-notify.
        let _stop_guard = StopOnPanic::new(handoff_ref);
        let mut sink = HandoffSink::new(handoff_ref);

        // The emit loop returns the song's outcome plus the EOS audio tail (if
        // any) for the submit thread to ship before it flushes.
        let (outcome, eos_tail): (DecodeResult, Option<(Vec<sp_ndi::AudioFrame>, i64)>) = 'emit: loop {
            // 1. Commands between boundaries (non-blocking).
            match cmd_rx.try_recv() {
                Ok(PipelineCommand::Shutdown) => {
                    log_song_summary(pacer, &summary_base, song_start, playlist_id, "shutdown");
                    break 'emit (DecodeResult::Shutdown, None);
                }
                Ok(PipelineCommand::Stop) => {
                    log_song_summary(pacer, &summary_base, song_start, playlist_id, "stop");
                    break 'emit (DecodeResult::Stopped, None);
                }
                Ok(PipelineCommand::Play {
                    video,
                    audio,
                    start_position_ms,
                }) => {
                    log_song_summary(pacer, &summary_base, song_start, playlist_id, "next");
                    break 'emit (
                        DecodeResult::NewPlay {
                            video,
                            audio,
                            start_position_ms,
                        },
                        None,
                    );
                }
                Ok(PipelineCommand::Pause) => {
                    *paused = true;
                    debug!(playlist_id, "paced: paused");
                }
                Ok(PipelineCommand::Resume) => {
                    *paused = false;
                    // Flush the audio buffer + reset the PLL (#148 rework, item 4):
                    // the pause backlog would otherwise overflow and leave audio
                    // seconds behind the video. The VIDEO anchor is left untouched —
                    // the frozen-standby held every boundary through the pause, so
                    // playback continues on the same wall grid.
                    pacer.audio_resume_reset();
                    debug!(playlist_id, "paced: resumed (audio buffer + PLL reset)");
                }
                Ok(PipelineCommand::Seek { position_ms }) => {
                    // Route the seek to the producer: it flushes the queue and bumps
                    // the epoch so any in-flight pre-seek frame is dropped, then seeks
                    // the decoder. Re-anchor the grid to the seek instant.
                    shared.request_seek(position_ms);
                    last_decoded_ms = position_ms;
                    pacer.anchor();
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    log_song_summary(
                        pacer,
                        &summary_base,
                        song_start,
                        playlist_id,
                        "disconnected",
                    );
                    break 'emit (DecodeResult::Shutdown, None);
                }
            }

            if *paused {
                // Fill EVERY grid boundary with the frozen last real frame so the
                // receiver stays `locked=` across a pause instead of dropping into
                // holes/underruns — one on-grid stamped frame per boundary via the
                // same Pacer sleep/emit machinery, handed off to the submit thread,
                // no audio (#147 fix-lane-2, change 2). Commands are serviced at the
                // loop top every iteration.
                match pacer.service_standby(Standby::FrozenLast, &mut sink) {
                    ServiceOutcome::Wait { until_100ns } => sleep_to_boundary(pacer, until_100ns),
                    _ => pacer.tick_wall(),
                }
                if should_run_heartbeat(last_heartbeat.elapsed()) {
                    emit_heartbeat_paced(
                        handoff_ref,
                        event_tx,
                        playlist_id,
                        PlaybackStateLabel::Paused,
                        last_heartbeat,
                        consecutive_bad_polls,
                        // Report the flag + accumulated counters, not default (#147
                        // change 7): a paced pipeline is `enabled=true` while paused.
                        pacer.stats(),
                        pacer.audio_stats(),
                        source_fps,
                        &mut hb_prev_total,
                        &mut hb_prev_instant,
                    );
                }
                continue;
            }

            // 2. Pop the frame the producer has ALREADY decoded ahead for the NEXT
            //    boundary — NO decode on the emit thread (#147 box test 4 fix). An
            //    empty queue (producer stall) pulls None, so the pacer repeats the
            //    last frame (never a hole). `prepare` still applies the presentation
            //    rule (drop-older / park-future) and pushes the audio grid.
            let target = pacer.next_boundary_100ns();
            pacer.prepare(target, || match shared.consumer_pop() {
                Some((frame, decoded_ms)) => {
                    last_decoded_ms = decoded_ms;
                    Some(frame)
                }
                None => None,
            });

            // 3. Sleep to the boundary, then HAND OFF the pre-decoded frame to the
            //    submit thread. `service` takes only the boundary audio chunk and
            //    the `HandoffSink` enqueues audio-before-video with the on-grid
            //    stamp — NO decode and NO blocking submit on this thread (#168).
            sleep_to_boundary(pacer, target);
            let outcome = pacer.service(|| None, &mut sink);

            match outcome {
                ServiceOutcome::Wait { until_100ns } => {
                    // A backward clock step re-latched the boundary; sleep to it.
                    sleep_to_boundary(pacer, until_100ns);
                }
                ServiceOutcome::Reanchored {
                    lag_slots,
                    until_100ns,
                } => {
                    // Playback fell irrecoverably behind (producer slower than the
                    // grid). The pacer re-anchored so the pre-decoded frame is due at
                    // `until_100ns`; sleep to it and continue (#147 lane 3, change 2).
                    warn!(playlist_id, lag_slots, "paced: lag exceeded — re-anchored");
                    sleep_to_boundary(pacer, until_100ns);
                }
                ServiceOutcome::Emitted | ServiceOutcome::Repeated | ServiceOutcome::Starved => {
                    pacer.tick_wall();

                    // One WARN per song when the audio buffer first overflows its 2 s
                    // cap (#148 rework, item 4) — kept in the pipeline layer so the
                    // buffer stays pure.
                    if pacer.audio_overflow_warn_needed() {
                        warn!(
                            playlist_id,
                            "paced: audio buffer overflowed its 2 s cap — dropping oldest audio"
                        );
                    }

                    if should_run_heartbeat(last_heartbeat.elapsed()) {
                        emit_heartbeat_paced(
                            handoff_ref,
                            event_tx,
                            playlist_id,
                            PlaybackStateLabel::Playing,
                            last_heartbeat,
                            consecutive_bad_polls,
                            pacer.stats(),
                            pacer.audio_stats(),
                            source_fps,
                            &mut hb_prev_total,
                            &mut hb_prev_instant,
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

                    // End the song when EITHER the producer drained normally (EOS +
                    // empty queue) with nothing parked in the pacer — the last frame
                    // has been shown — OR the producer thread has EXITED without
                    // signalling EOS (a panic after the open phase). The latter is
                    // symmetric with the open-phase liveness check (`open_rx` →
                    // Disconnected): `SharedQueue` is an Arc<Mutex>, not a channel, so
                    // a dead producer would otherwise leave `is_drained()` false
                    // forever and freeze the wall on a repeat with no auto-advance.
                    // Ending (not erroring) lets the playlist move to the next song —
                    // the resilient choice on a box with a driver-timeout history.
                    let producer_dead = producer.is_finished() && !shared.is_drained();
                    if (shared.is_drained() && !pacer.has_pending()) || producer_dead {
                        if producer_dead {
                            error!(
                                playlist_id,
                                "paced: decode producer exited without EOS — ending song"
                            );
                        } else {
                            info!(playlist_id, "paced: video decode complete");
                        }
                        // Hand the remaining buffered audio (zero-filled to a full
                        // boundary, raw wall timecode) to the submit thread so the
                        // last <1 boundary of audio is not dropped (#148 rework,
                        // item 4). The submit thread ships it after draining.
                        let tail = pacer.take_eos_tail();
                        let tail_msg = if tail.is_empty() {
                            None
                        } else {
                            Some((tail, pacer.now_100ns()))
                        };
                        let reason = if producer_dead {
                            "producer-died"
                        } else {
                            "ended"
                        };
                        log_song_summary(pacer, &summary_base, song_start, playlist_id, reason);
                        break 'emit (DecodeResult::Ended, tail_msg);
                    }
                }
            }
        };

        // Signal the submit thread: drain the handoff, ship the EOS tail, flush
        // the async double-buffer, and exit. The scope JOINS it here, so the
        // submitter's `prev_frame` is released before this function returns and
        // the outer loop reuses the submitter (a black frame / the next song).
        handoff_ref.stop_with_tail(eos_tail);
        let _ = submit_join.join();
        outcome
    });

    // Stop the producer + join it so the decoder drops on its own STA thread
    // before this pipeline call returns (#147). A backpressured / post-EOS
    // producer wakes on the stop signal; a mid-decode producer sees stop on its
    // next push — so the join completes within ~one decode.
    shared.stop();
    let _ = producer.join();
    result
}
