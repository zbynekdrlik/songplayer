//! Windows-only paced heartbeat glue (#168 output-side split).
//!
//! The submit side itself — the emit→submit [`SharedHandoff`], the
//! [`HandoffSink`](crate::playback::paced_output::HandoffSink) the pacer emits
//! through and the submit consumer — is the cross-platform
//! `paced_output.rs`, so Linux tests drive it over `MockNdiBackend`. What stays
//! here is the paced health heartbeat, which reads the submit-side snapshot
//! (the submit thread owns the submitter) and sends the Windows pipeline's
//! `PipelineEvent`. Box-verified, `mutants::skip` glue.

use std::time::Instant;

use crate::playback::ndi_health::{AudioStats, PacingStats, PlaybackStateLabel};
use crate::playback::paced_output::SharedHandoff;
use crate::playback::pipeline::{PipelineEvent, classify_bad_poll};
use crate::playback::submit_handoff::merge_pacing_stats;

/// Emit a health heartbeat on the PACED path (#168), reading the submit-thread
/// snapshot instead of the `FrameSubmitter` (which the submit thread owns for
/// the song). Same `HealthSnapshot` event + bad-poll classification as
/// `pipeline::emit_heartbeat`, but `late_frames` / `max_late_us` / `iter_p99` /
/// `dropped` come from the merged pacer+submit stats, and the frame / connection
/// / last-submit fields come from the submit snapshot. `nominal_fps` is the fixed
/// genlock grid (the paced submitter carries `GENLOCK_GRID_FPS/1`); `source_fps`
/// is the DECODER's rate (#168 r6b), passed in by the caller because the submit
/// thread owns the submitter here — the lock rule reads `source_fps`, not the
/// grid-valued `nominal_fps`.
#[cfg_attr(test, mutants::skip)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_heartbeat_paced(
    handoff: &SharedHandoff,
    event_tx: &tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
    state: PlaybackStateLabel,
    last_heartbeat: &mut Instant,
    consecutive_bad_polls: &mut u32,
    pacer_stats: PacingStats,
    audio: AudioStats,
    source_fps: f32,
    prev_total: &mut u64,
    prev_instant: &mut Instant,
) {
    let (submit, connections, last_submit_ts, paced_submit) = handoff.snapshot();
    let pacing = merge_pacing_stats(pacer_stats, &submit, paced_submit);

    let total = submit.submitted;
    let now = Instant::now();
    let window_secs = now.duration_since(*prev_instant).as_secs_f32().max(0.001);
    let window_frames = total.saturating_sub(*prev_total);
    let observed_fps = window_frames as f32 / window_secs;
    let nominal_fps = sp_core::genlock::GENLOCK_GRID_FPS as f32;

    let bad = classify_bad_poll(
        &state,
        connections,
        observed_fps,
        nominal_fps,
        last_submit_ts,
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
            frames_submitted_total: total,
            frames_submitted_last_5s: window_frames as u32,
            observed_fps,
            nominal_fps,
            source_fps,
            last_submit_ts,
            last_heartbeat_ts: now,
            consecutive_bad_polls: *consecutive_bad_polls,
            reported_state: state,
            pacing,
            audio,
            // #168 r2: the paced path has no SDK-clocked decode loop, so its
            // decode/submit/audio STAGE maxima stay 0 — but the per-call
            // send_video_async gauge DOES apply, so carry it into the SAME
            // `submit_call_us_max`/`_p99` fields the SDK-clocked `pipeline:
            // loop-stats` line uses (identical naming), so a pacing-ON box test
            // reads the per-frame SDK submit cost per minute from that line.
            // #147 r9: + SongPlayer's own page faults/min + working set (MiB),
            // sampled process-wide at most once a minute (`proc_mem::gauge`).
            loop_stats: crate::playback::loop_stats::LoopStats {
                submit_call_us_max: paced_submit.submit_call_us_max,
                submit_call_us_p99: paced_submit.submit_call_us_p99,
                proc_mem: crate::playback::proc_mem::gauge(),
                ..Default::default()
            },
        },
    ));
    *prev_total = total;
    *prev_instant = now;
    *last_heartbeat = now;
}
