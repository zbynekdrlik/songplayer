//! Windows-only idle wait for the pipeline outer loop (#147 fix-lane-2).
//!
//! Owns the whole no-song idle tick for BOTH `genlock_pacing` states:
//!
//! - **ON:** fill EVERY grid boundary with black + one silent audio block so the
//!   receiver stays `locked=` between songs instead of seeing holes / underruns —
//!   one on-grid stamped frame per boundary, driven by the pure
//!   [`Pacer::service_standby`](crate::playback::pacer::Pacer::service_standby).
//!   Since the #147 standby same-path fix (design comment 5841796900) the idle
//!   frame leaves through the SAME path as a playing frame: the #168
//!   [`HandoffSink`] → submit thread → `submit_frame_at_boundary_owned` (audio
//!   first, then the async NV12 send at the stamped boundary), so the receiver
//!   sees one constant A/V cadence and phase, idle or playing. The outer loop
//!   enters this at once when paced (`pacer_sink::idle_poll`).
//! - **OFF (legacy):** the plain single idle heartbeat the outer loop always did
//!   (the moved `run_heartbeat_outer`), leaving pacing to the 5 s `recv_timeout`.
//!
//! The scheduling DECISION (Wait vs emit, the on-grid stamp, catch-up/resync,
//! the silent block) all lives in the cross-platform, Linux-tested `Pacer`; only
//! the submit-thread scope, the real NDI submit and the command peek are Windows
//! glue here.

use std::time::Instant;

use crossbeam_channel::Receiver;

use crate::playback::ndi_health::PlaybackStateLabel;
use crate::playback::pacer::{Pacer, ServiceOutcome, Standby};
use crate::playback::pipeline::{
    PipelineCommand, PipelineEvent, emit_heartbeat, should_run_heartbeat,
};
use crate::playback::pipeline_paced::sleep_to_boundary;
use crate::playback::pipeline_paced_submit::{
    HandoffSink, SharedHandoff, StopOnPanic, emit_heartbeat_paced, run_submit_consumer,
};
use crate::playback::submit_handoff::SUBMIT_HANDOFF_BOUND;
use crate::playback::submitter::FrameSubmitter;

/// The idle/no-song standby resolution (1080p). The idle black frame is built
/// ONCE per pipeline (cached in the `FrameSubmitter`) and submitted by shared
/// reference every boundary (#203, #147).
const IDLE_W: u32 = 1920;
const IDLE_H: u32 = 1080;

/// The outer-loop idle wait (no song loaded). With `genlock_pacing` ON, fill
/// every grid boundary with black + silence through the #168 submit thread until
/// a command is queued, then stop + join that thread (it flushes the async
/// holdover) and return so the outer loop's next `recv` picks the command up;
/// with it OFF, emit one idle heartbeat and return (the legacy behaviour).
/// Play/Seek re-anchor the grid via `Pacer::anchor` inside the decode loop, so
/// the idle-fill's boundary state is reset cleanly when playback resumes.
#[cfg_attr(test, mutants::skip)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_idle_wait(
    genlock_pacing: bool,
    submitter: &mut FrameSubmitter<sp_ndi::RealNdiBackend>,
    pacer: &mut Pacer,
    cmd_rx: &Receiver<PipelineCommand>,
    event_tx: &tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
    paused: bool,
    last_heartbeat: &mut Instant,
    consecutive_bad_polls: &mut u32,
) {
    let state = if paused {
        PlaybackStateLabel::Paused
    } else {
        PlaybackStateLabel::Idle
    };

    if !genlock_pacing {
        // Legacy path: one idle heartbeat per 5 s outer timeout, unchanged.
        emit_heartbeat(
            submitter,
            event_tx,
            playlist_id,
            state,
            last_heartbeat,
            consecutive_bad_polls,
            pacer.stats(),
            pacer.audio_stats(),
            // Idle: no SDK decode loop → no stage telemetry.
            crate::playback::loop_stats::LoopStageStats::default(),
        );
        return;
    }

    // The ONE idle black, cached in the submitter for the pipeline's life; every
    // boundary submits it by reference (a refcount bump, zero pixel copies, #203).
    let black = submitter.standby_black_nv12(IDLE_W, IDLE_H);
    // #147 standby same-path: the idle boundary leaves through the SAME #168
    // handoff + submit thread as a playing frame (`decode_and_send_paced`). The
    // submit thread borrows the submitter for this idle stretch (SDK per-instance
    // affinity + the async holdover stay single-threaded) and is joined, after a
    // flush, before the outer loop touches the submitter again.
    let handoff = SharedHandoff::new(SUBMIT_HANDOFF_BOUND);
    let handoff_ref = &handoff;
    // Heartbeat window baselines over the submit-side frame count (the frames
    // that actually left the box), as on the playing path.
    let mut hb_prev_total: u64 = 0;
    let mut hb_prev_instant = Instant::now();
    // Idle has no decoder: report the grid rate, which is what the idle
    // heartbeat reported before (`submitter.nominal_fps()` = the grid here).
    let idle_source_fps = sp_core::genlock::GENLOCK_GRID_FPS as f32;

    std::thread::scope(|s| {
        let sub: &mut FrameSubmitter<sp_ndi::RealNdiBackend> = submitter;
        let submit_join = s.spawn(move || run_submit_consumer(sub, handoff_ref, playlist_id));
        // A panic in the fill must still stop the submit thread, or the scope's
        // join deadlocks on the parked consumer (#168 review).
        let _stop_guard = StopOnPanic::new(handoff_ref);
        let mut sink = HandoffSink::new(handoff_ref);

        // Fill boundaries until a command is queued; the caller then receives it.
        while cmd_rx.is_empty() {
            let standby = Standby::Black {
                width: IDLE_W,
                height: IDLE_H,
                stride: IDLE_W,
                video: &black,
            };
            match pacer.service_standby(standby, &mut sink) {
                ServiceOutcome::Wait { until_100ns } => sleep_to_boundary(pacer, until_100ns),
                _ => pacer.tick_wall(),
            }
            if should_run_heartbeat(last_heartbeat.elapsed()) {
                emit_heartbeat_paced(
                    handoff_ref,
                    event_tx,
                    playlist_id,
                    state.clone(),
                    last_heartbeat,
                    consecutive_bad_polls,
                    // A paced pipeline is `enabled=true` while idle (#147 change 7).
                    pacer.stats(),
                    pacer.audio_stats(),
                    idle_source_fps,
                    &mut hb_prev_total,
                    &mut hb_prev_instant,
                );
            }
        }

        // Drain the handoff, flush the async holdover, and join the submit thread
        // before the outer loop (or the next song) reuses the submitter.
        handoff_ref.stop_with_tail(None);
        let _ = submit_join.join();
    });
}
