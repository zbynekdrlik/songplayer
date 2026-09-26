//! The `SP-program` NDI output (#209, B1 of EPIC #174).
//!
//! [`ProgramOutput`] owns the program's own paced [`FrameSubmitter`] (NDI
//! sender [`PROGRAM_NDI_NAME`], `clock_video = false` like every paced output)
//! and submits each [`ProgramJob`] the [`ProgramBus`] queued: a forwarded source
//! boundary unchanged (the same `Arc` frame, the same audio block, the same
//! stamps), or the program's own #147 standby pair (the cached NV12 black + one
//! 1600-sample silent block) for a missed boundary — audio first, then the async
//! video, exactly like a source boundary.
//!
//! [`run_program_loop`] is the sender thread: once per boundary (1 ms after it,
//! [`next_check_wait`]) it releases the missed boundaries, and it submits every
//! queued job as soon as it is queued. `start_program` (an
//! `impl PlaybackEngine` split out of `mod.rs` for the 1000-line cap) restores
//! the persisted source, installs the process-wide bus for the paced submit
//! threads, and on Windows starts the thread on the engine's NDI backend. It
//! runs AFTER the #196 startup senders, so the per-playlist name→port order is
//! unchanged.

use std::sync::Arc;
use std::time::Duration;

use sp_core::genlock::audio::samples_per_boundary;
use sp_core::genlock::{
    GENLOCK_GRID_FPS, floor_boundary_100ns, lag_slots_100ns, strict_next_boundary_100ns,
};
use sp_ndi::{AudioFrame, NdiBackend, NdiSender};
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::playback::program_bus::{
    PROGRAM_NDI_NAME, ProgramBus, ProgramJob, Take, install, restore_selected_source,
};
use crate::playback::submitter::FrameSubmitter;
use crate::playback::wallclock::WallClock;

/// The program standby black resolution (1080p, the paced idle size).
pub const PROGRAM_STANDBY_W: u32 = 1920;
pub const PROGRAM_STANDBY_H: u32 = 1080;

/// The program's silent block: 48 kHz stereo, one grid slot.
const PROGRAM_AUDIO_RATE_HZ: u32 = 48_000;
const PROGRAM_AUDIO_CHANNELS: u32 = 2;

/// Poll the `SP-program` receiver connection count every this many submitted
/// pairs (~1 s at 30 fps), as the source submit threads do.
const CONN_POLL_EVERY: u32 = 30;

/// The sender thread checks for missed boundaries this long after each
/// boundary (100 ns units, 1 ms).
pub const CHECK_AFTER_BOUNDARY_100NS: i64 = 10_000;

/// The program's paced sender plus its standby pair.
pub struct ProgramOutput<B: NdiBackend> {
    submitter: FrameSubmitter<B>,
    /// The ONE silent block every standby pair carries (built once).
    silence: Vec<AudioFrame>,
    standby_w: u32,
    standby_h: u32,
}

impl<B: NdiBackend> ProgramOutput<B> {
    /// Wrap the program's NDI sender. The standby black is `standby_w` ×
    /// `standby_h` NV12 ([`PROGRAM_STANDBY_W`] × [`PROGRAM_STANDBY_H`] in
    /// production).
    pub fn new(sender: NdiSender<B>, standby_w: u32, standby_h: u32) -> Self {
        let mut submitter = FrameSubmitter::new(sender, GENLOCK_GRID_FPS as i32, 1);
        submitter.set_paced(true);
        let spc = samples_per_boundary(PROGRAM_AUDIO_RATE_HZ as i64, GENLOCK_GRID_FPS);
        let silence = vec![AudioFrame {
            data: vec![0.0; spc * PROGRAM_AUDIO_CHANNELS as usize],
            channels: PROGRAM_AUDIO_CHANNELS,
            sample_rate: PROGRAM_AUDIO_RATE_HZ,
            timecode_100ns: None,
        }];
        Self {
            submitter,
            silence,
            standby_w,
            standby_h,
        }
    }

    /// Submit one program boundary and return its stamp. A forwarded source
    /// job keeps its own stamps; a standby pair is stamped on its boundary with
    /// the audio stamped `audio_now_100ns` (the emit instant, like #147
    /// standby audio).
    pub fn submit(&mut self, job: ProgramJob, audio_now_100ns: i64) -> i64 {
        match job {
            ProgramJob::Source(job) => {
                let stamp = job.video_tc_100ns;
                self.submitter.submit_frame_at_boundary_owned(
                    job.width,
                    job.height,
                    job.stride,
                    job.video,
                    &job.audio,
                    stamp,
                    job.audio_tc_100ns,
                );
                stamp
            }
            ProgramJob::Standby { stamp_100ns } => {
                let (w, h) = (self.standby_w, self.standby_h);
                let black = self.submitter.standby_black_nv12(w, h);
                self.submitter.submit_frame_at_boundary_owned(
                    w,
                    h,
                    w,
                    black,
                    &self.silence,
                    stamp_100ns,
                    audio_now_100ns,
                );
                stamp_100ns
            }
        }
    }

    /// Current `SP-program` receiver connection count.
    pub fn connections(&self) -> i32 {
        self.submitter.sender().get_no_connections(0)
    }

    /// Release the async holdover (the sender thread's exit).
    pub fn flush(&mut self) {
        self.submitter.flush();
    }
}

/// Most wall ticks one wake may owe (a long stall catches up in bounded work).
pub const MAX_TICKS_PER_WAKE: i64 = 1;

/// Ticks the program's [`WallClock`] once per grid boundary PASSED — the pacer's
/// cadence (`Pacer::tick_wall`, once per serviced boundary) — never once per
/// loop wake. The wall re-anchors every 100 ticks and slews a UTC step in at
/// ≤ 1 ms per re-anchor; the sender wakes about twice per boundary, so ticking
/// per wake would slew twice as fast as the stamp walls and, after a forward
/// step, run ahead of the owner's stamps until the fill grace black-fills its
/// boundaries (#209 review). Same cadence = same slew = one clock domain.
#[derive(Debug, Default)]
pub struct BoundaryTicker {
    last: Option<i64>,
}

impl BoundaryTicker {
    /// How many wall ticks are owed at `now_100ns`: the grid boundaries passed
    /// since the last call that owed some (0 on the first call, which only
    /// anchors; 0 on a backward clock read), at most [`MAX_TICKS_PER_WAKE`].
    pub fn advance(&mut self, now_100ns: i64) -> i64 {
        let floor = floor_boundary_100ns(now_100ns, GENLOCK_GRID_FPS);
        let owed = self
            .last
            .map_or(0, |last| lag_slots_100ns(last, floor, GENLOCK_GRID_FPS))
            .min(MAX_TICKS_PER_WAKE);
        if owed > 0 || self.last.is_none() {
            self.last = Some(floor);
        }
        owed
    }
}

/// How long the sender thread waits for a queued boundary before it checks
/// for missed ones again: until [`CHECK_AFTER_BOUNDARY_100NS`] past the next
/// grid boundary.
pub fn next_check_wait(now_100ns: i64) -> Duration {
    let due = strict_next_boundary_100ns(now_100ns, GENLOCK_GRID_FPS) + CHECK_AFTER_BOUNDARY_100NS;
    Duration::from_nanos((due - now_100ns).max(0) as u64 * 100)
}

/// The `SP-program` sender thread: release missed boundaries once per
/// boundary, submit every queued boundary, poll the receiver count ~1/s. Exits
/// (after a flush) once the bus is stopped and drained.
#[cfg_attr(test, mutants::skip)]
pub fn run_program_loop<B: NdiBackend>(
    out: &mut ProgramOutput<B>,
    bus: &ProgramBus,
    wall: &mut WallClock,
) {
    let mut since_conn_poll = CONN_POLL_EVERY; // poll on the first pair
    let mut ticker = BoundaryTicker::default();
    loop {
        let now = wall.now_100ns();
        for _ in 0..ticker.advance(now) {
            wall.tick(); // once per grid boundary, like the pacer walls
        }
        bus.release_due(now);
        match bus.take_timeout(next_check_wait(now)) {
            Take::Job(job) => {
                let stamp = out.submit(job, wall.now_100ns());
                bus.record_submitted(stamp);
                since_conn_poll += 1;
                if since_conn_poll >= CONN_POLL_EVERY {
                    bus.set_connections(out.connections());
                    since_conn_poll = 0;
                }
            }
            Take::Idle => {}
            Take::Stopped => break,
        }
    }
    out.flush();
    info!(
        ndi_name = PROGRAM_NDI_NAME,
        "program output: stopped + flushed"
    );
}

impl super::PlaybackEngine {
    /// #209: restore the persisted program source, install the process-wide
    /// bus the paced submit threads offer to, start the `SP-program` sender
    /// thread (Windows, on the engine's NDI backend), and stop it on shutdown.
    /// Call once, after the #196 startup senders.
    #[cfg_attr(test, mutants::skip)]
    pub async fn start_program(&self, bus: Arc<ProgramBus>, shutdown: &broadcast::Sender<()>) {
        let mut shutdown = shutdown.subscribe();
        restore_selected_source(&self.pool, &bus).await;
        if !install(bus.clone()) {
            warn!("program bus: a bus was already installed — keeping the first one");
        }
        #[cfg(windows)]
        spawn_program_thread(self.ndi_backend.clone(), bus.clone());
        tokio::spawn(async move {
            let _ = shutdown.recv().await;
            bus.stop();
        });
    }
}

/// Windows: create the `SP-program` sender on the shared NDI backend and run
/// [`run_program_loop`] on its own thread.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
fn spawn_program_thread(backend: Option<super::pipeline::SharedNdiBackend>, bus: Arc<ProgramBus>) {
    let Some(backend) = backend else {
        warn!("NDI SDK not available — no SP-program output");
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("program-output".into())
        .spawn(move || {
            crate::playback::pipeline_paced::request_high_res_timer();
            let sender = match NdiSender::new_with_clocking(backend, PROGRAM_NDI_NAME, false, false)
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(%e, "failed to create the SP-program NDI sender");
                    return;
                }
            };
            info!(ndi_name = PROGRAM_NDI_NAME, "program output thread started");
            let mut out = ProgramOutput::new(sender, PROGRAM_STANDBY_W, PROGRAM_STANDBY_H);
            let mut wall = WallClock::system();
            run_program_loop(&mut out, &bus, &mut wall);
        });
    if let Err(e) = spawned {
        tracing::error!(%e, "failed to spawn the SP-program output thread");
    }
}

#[cfg(test)]
#[path = "program_output_tests.rs"]
mod tests;
