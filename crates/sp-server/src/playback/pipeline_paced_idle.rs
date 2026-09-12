//! Windows-only idle wait for the pipeline outer loop (#147 fix-lane-2).
//!
//! Owns the whole no-song idle tick for BOTH `genlock_pacing` states:
//!
//! - **ON:** fill EVERY grid boundary with black so the receiver stays `locked=`
//!   between songs instead of seeing holes / underruns — one on-grid stamped
//!   frame per boundary, driven by the pure
//!   [`Pacer::service_standby`](crate::playback::pacer::Pacer::service_standby),
//!   exactly the paused-branch treatment in
//!   [`pipeline_paced`](crate::playback::pipeline_paced) for the no-song state.
//! - **OFF (legacy):** the plain single idle heartbeat the outer loop always did
//!   (the moved `run_heartbeat_outer`), leaving pacing to the 5 s `recv_timeout`.
//!
//! The scheduling DECISION (Wait vs emit, the on-grid stamp, catch-up/resync)
//! all lives in the cross-platform, Linux-tested `Pacer`; only the MF-less black
//! frame build, the real NDI submit, and the command peek are Windows glue here.

use std::time::Instant;

use crossbeam_channel::Receiver;

use crate::playback::ndi_health::PlaybackStateLabel;
use crate::playback::pacer::{PacedFrame, Pacer, ServiceOutcome, Standby};
use crate::playback::pipeline::{
    PipelineCommand, PipelineEvent, emit_heartbeat, should_run_heartbeat,
};
use crate::playback::pipeline_paced::sleep_to_boundary;
use crate::playback::submitter::FrameSubmitter;

/// A neutral-black NV12 frame (Y = studio black 16, interleaved UV = 128) used
/// as the idle/no-song standby picture. Matches the black `send_black_bgra`
/// standby visually, but is NV12 so it rides the paced submit path.
fn black_nv12(width: u32, height: u32) -> PacedFrame {
    let y = (width as usize) * (height as usize);
    let mut data = vec![16u8; y];
    data.resize(y + y / 2, 128u8);
    PacedFrame {
        pts_ns: 0,
        width,
        height,
        stride: width,
        video: data,
        audio: Vec::new(),
    }
}

/// The outer-loop idle wait (no song loaded). With `genlock_pacing` ON, fill
/// every grid boundary with black until a command is queued, then return so the
/// outer loop's next `recv` picks it up; with it OFF, emit one idle heartbeat
/// and return (the legacy behaviour). Play/Seek re-anchor the grid via
/// `Pacer::anchor` inside the decode loop, so the idle-fill's boundary state is
/// reset cleanly when playback resumes.
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
        );
        return;
    }

    let black = black_nv12(1920, 1080);
    // Fill boundaries until a command is queued; the caller then receives it.
    while cmd_rx.is_empty() {
        match pacer.service_standby(Standby::Black(&black), submitter) {
            ServiceOutcome::Wait { until_100ns } => sleep_to_boundary(pacer, until_100ns),
            _ => pacer.tick_wall(),
        }
        if should_run_heartbeat(last_heartbeat.elapsed()) {
            emit_heartbeat(
                submitter,
                event_tx,
                playlist_id,
                state.clone(),
                last_heartbeat,
                consecutive_bad_polls,
                // A paced pipeline is `enabled=true` while idle (#147 change 7).
                pacer.stats(),
            );
        }
    }
}
