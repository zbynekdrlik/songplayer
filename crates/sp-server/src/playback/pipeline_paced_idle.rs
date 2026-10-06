//! Windows-only idle wait for the pipeline outer loop (#147 fix-lane-2).
//!
//! Fills EVERY grid boundary with black + one silent audio block while no
//! song is loaded, so the program bus gets one constant A/V cadence and phase
//! from this playlist, idle or playing, driven by the pure
//! [`Pacer::service_standby`](crate::playback::pacer::Pacer::service_standby).
//! Since the #147 standby same-path fix (design comment 5841796900) the idle
//! frame leaves through the SAME path as a playing frame: the #168
//! `HandoffSink` → the pipeline's paced output (#221 lane 3: on to the program
//! bus alone). The outer loop enters this at once. The output's consumer
//! thread is the pipeline's (#147, design record 5845527884): an idle stretch
//! only attaches a feeder and continues right after the last serviced
//! boundary.
//!
//! The scheduling DECISION (Wait vs emit, the on-grid stamp, catch-up/resync,
//! the silent block) all lives in the cross-platform, Linux-tested `Pacer`,
//! and the output side in the Linux-tested `paced_output.rs`; only the feeder
//! attach, the real sleep and the command peek are Windows glue here.

use std::time::Instant;

use crossbeam_channel::Receiver;

use crate::playback::ndi_health::PlaybackStateLabel;
use crate::playback::paced_output::{InstalledBus, PacedFeed, PipelineOutput};
use crate::playback::pacer::{Pacer, ServiceOutcome, StandbyBlack};
use crate::playback::pipeline::{PipelineCommand, PipelineEvent, should_run_heartbeat};
use crate::playback::pipeline_paced::sleep_to_boundary;
use crate::playback::pipeline_paced_submit::emit_heartbeat_paced;

/// The idle/no-song standby resolution (1080p). The idle black frame is built
/// ONCE per pipeline (cached in its `PipelineOutput`) and handed over by
/// shared reference every boundary (#203, #147).
pub(crate) const IDLE_W: u32 = 1920;
pub(crate) const IDLE_H: u32 = 1080;

/// The outer-loop idle wait (no song loaded): fill every grid boundary with
/// black + silence through the pipeline's paced output until a command is
/// queued, then detach (the consumer keeps servicing the grid, #147) and
/// return so the outer loop picks the command up. The next song's pre-roll
/// continues right after the last serviced boundary.
#[cfg_attr(test, mutants::skip)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_idle_wait(
    output: &mut PipelineOutput<InstalledBus>,
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

    // The ONE idle black, cached in the output for the pipeline's life; every
    // boundary hands it over by reference (a refcount bump, zero pixel copies,
    // #203).
    let black = output.standby_black_nv12(IDLE_W, IDLE_H);
    // #147 standby same-path: the idle boundary leaves through the SAME #168
    // handoff + paced output as a playing frame (`decode_and_send_paced`). Its
    // consumer is the pipeline's (#147, design record 5845527884): this idle
    // stretch attaches a feeder, continues right after the last boundary the
    // output serviced, and detaches when a command is queued — no join, so no
    // boundary goes unserviced before the next scope.
    let handoff = output.handoff(IDLE_W, IDLE_H);
    let feed = PacedFeed::attach(&handoff);
    if let Some(last) = feed.continue_after_100ns() {
        pacer.continue_grid_after(last);
    }
    // Heartbeat window baselines over the output's delivered count; the
    // consumer's counters live for the pipeline, so start from where they are.
    let mut hb_prev_total: u64 = handoff.submitted();
    let mut hb_prev_instant = Instant::now();
    // Idle has no decoder: report the grid rate as the source rate.
    let idle_source_fps = sp_core::genlock::GENLOCK_GRID_FPS as f32;
    // A resync in the idle stretch means more than 8 boundaries sat between the
    // output's last serviced stamp and this fill (a hung box — the paced output
    // services every boundary between scopes, #147): a real stamp hole.
    let resyncs_before = pacer.stats().resyncs;
    let mut sink = feed.sink();

    // Fill boundaries until a command is queued; the caller then receives it.
    while cmd_rx.is_empty() {
        // The same black constructor the song-start pre-roll uses.
        let standby = StandbyBlack {
            width: IDLE_W,
            height: IDLE_H,
            stride: IDLE_W,
            video: &black,
        }
        .standby();
        match pacer.service_standby(standby, &mut sink) {
            ServiceOutcome::Wait { until_100ns } => sleep_to_boundary(pacer, until_100ns),
            _ => pacer.tick_wall(),
        }
        if should_run_heartbeat(last_heartbeat.elapsed()) {
            emit_heartbeat_paced(
                &handoff,
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

    // Hand the grid back to the paced output: it services every boundary until
    // the next scope (the song the queued command starts, or the next idle
    // stretch) attaches.
    drop(feed);
    let gap_resyncs = pacer.stats().resyncs.saturating_sub(resyncs_before);
    if gap_resyncs > 0 {
        tracing::warn!(
            playlist_id,
            gap_resyncs,
            "paced idle: > 8 boundaries went unserviced before or during the idle fill (grid resync)"
        );
    }
}
