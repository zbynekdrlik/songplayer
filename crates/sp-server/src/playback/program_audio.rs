//! The program's audio side on its own `program-audio` thread (#289 step 2,
//! design comments 6814927800 and 7180172596).
//!
//! The bus decides every boundary ONCE (`ProgramCore::commit`: a forward, a
//! fill, a fade's mix). Since step 2 the paced sources hand each committed
//! pair to the bus one slot before its boundary (`pacer_commit.rs`), and
//! with the audio side on (`ProgramBus::with_audio_ahead`) the commit MOVES
//! the boundary's audio out of the job into an [`AudioJob`]
//! ([`take_audio`]). Two readers then read that one decision:
//!
//! - the `program-audio` thread ([`run_audio_loop`]) takes the audio jobs in
//!   commit order, and its [`AudioSide`] crossfades a window boundary, runs
//!   the program's ONE peak limiter, notes the limiter's overs, hands the
//!   block to every audio output and keeps the limited block in the
//!   stamp-keyed [`AudioLine`];
//! - the `SP-program` sender takes the job, holds it to its stamp
//!   ([`hold_to_stamp`]) and takes the boundary's limited block from the
//!   line for its NDI submit, so `SP-program` still carries the block of B
//!   with the picture of B, at B.
//!
//! So a long video side of B never delays the audio of B + 1. The outputs
//! still schedule on the stamp (+ 66.7 ms): an earlier hand-off only grows
//! their margin (`health.audio` on `GET /api/v1/program`).
//!
//! The tests' `ProgramOutput` (no line) runs the same [`AudioSide`] inline,
//! which is the sender before step 2, block for block
//! (`program_output_tests_audio.rs`).

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use sp_core::genlock::audio::samples_per_boundary;
use sp_core::genlock::{GENLOCK_GRID_FPS, strict_next_boundary_100ns};
use sp_decoder::PeakLimiter;
use sp_ndi::AudioFrame;

use crate::playback::audio_out::AudioOutputs;
use crate::playback::audio_out_block::{ProgramBlock, is_program_block};
use crate::playback::pacer::AUDIO_LEAD_SLOTS;
use crate::playback::program_bus::{PROGRAM_QUEUE_BOUND, ProgramBus, ProgramJob};
use crate::playback::program_max_send::SendClock;
use crate::playback::program_output_timing::TIMING_BUCKET_BOUNDARIES;
use crate::playback::program_transition::{AudioFormat, mix_audio_block};
use crate::playback::stat_window::{TwoBucketWorst, Worst};
use crate::playback::submit_handoff::SubmitJob;

/// The program's audio: 48 kHz stereo, one grid slot per block.
pub const PROGRAM_AUDIO_RATE_HZ: u32 = 48_000;
pub const PROGRAM_AUDIO_CHANNELS: u32 = 2;

// #210: the VBAN output carries exactly the program's audio format.
const _: () = assert!(
    PROGRAM_AUDIO_RATE_HZ as i64 == crate::playback::vban_packet::VBAN_SAMPLE_RATE_HZ
        && PROGRAM_AUDIO_CHANNELS as usize == crate::playback::vban_packet::VBAN_CHANNELS
);

/// The line holds the limited blocks the sender has not taken yet: about
/// one in steady state, a full program queue's worth behind a slow video
/// side; over this the OLDEST is dropped (counted).
pub const LINE_BOUND: usize = PROGRAM_QUEUE_BOUND + 2;

/// How long the sender waits for a boundary's limited block it does not
/// find yet (a fill committed as the sender took it: the audio thread
/// limits it in µs).
pub const LINE_WAIT: Duration = Duration::from_millis(5);

/// How long the audio thread waits for a job before it checks for a stop.
pub const AUDIO_IDLE_WAIT: Duration = Duration::from_millis(100);

/// The furthest a job may lie ahead of the sender's wall to be held to its
/// stamp: the lead and one slot (the pacer's stranded rule). A job further
/// ahead (a backward step) goes at once.
pub const HOLD_MAX_100NS: i64 = (AUDIO_LEAD_SLOTS + 1) * 333_334;

/// Audio frames per boundary (1600).
pub fn program_spc() -> usize {
    samples_per_boundary(PROGRAM_AUDIO_RATE_HZ as i64, GENLOCK_GRID_FPS)
}

/// The program's one silent block (one boundary, 48 kHz stereo).
pub fn silent_block() -> Vec<AudioFrame> {
    vec![AudioFrame {
        data: vec![0.0; program_spc() * PROGRAM_AUDIO_CHANNELS as usize],
        channels: PROGRAM_AUDIO_CHANNELS,
        sample_rate: PROGRAM_AUDIO_RATE_HZ,
        timecode_100ns: None,
    }]
}

/// One boundary's audio side, as the bus decided it.
#[derive(Debug)]
pub enum AudioJob {
    /// A forwarded source pair's own block(s).
    Source {
        stamp_100ns: i64,
        audio: Vec<AudioFrame>,
    },
    /// The program's standby silence (a fill, a mix with neither side).
    Standby { stamp_100ns: i64 },
    /// A window boundary: both sides' blocks and the boundary's place in
    /// the window (`MixJob::sample_span`).
    Mix {
        stamp_100ns: i64,
        from: Option<AudioFrame>,
        to: Option<AudioFrame>,
        first: u64,
        total: u64,
    },
}

impl AudioJob {
    /// The boundary this audio belongs to.
    pub fn stamp_100ns(&self) -> i64 {
        match self {
            AudioJob::Source { stamp_100ns, .. }
            | AudioJob::Standby { stamp_100ns }
            | AudioJob::Mix { stamp_100ns, .. } => *stamp_100ns,
        }
    }
}

/// A window side's first block, moved out of its pair.
fn first_block(side: &mut Option<SubmitJob>) -> Option<AudioFrame> {
    let job = side.as_mut()?;
    std::mem::take(&mut job.audio).into_iter().next()
}

/// Move `job`'s audio out into its [`AudioJob`]; the job keeps its picture.
/// A mix with neither side is the standby (the sender shows it as one).
pub fn take_audio(job: &mut ProgramJob) -> AudioJob {
    match job {
        ProgramJob::Source(source) => AudioJob::Source {
            stamp_100ns: source.video_tc_100ns,
            audio: std::mem::take(&mut source.audio),
        },
        ProgramJob::Standby { stamp_100ns } => AudioJob::Standby {
            stamp_100ns: *stamp_100ns,
        },
        ProgramJob::Mix(mix) if mix.from.is_none() && mix.to.is_none() => AudioJob::Standby {
            stamp_100ns: mix.stamp_100ns,
        },
        ProgramJob::Mix(mix) => {
            let (first, total) = mix.sample_span(program_spc());
            AudioJob::Mix {
                stamp_100ns: mix.stamp_100ns,
                from: first_block(&mut mix.from),
                to: first_block(&mut mix.to),
                first,
                total,
            }
        }
    }
}

/// One boundary's audio after the limiter, as the outputs got it.
#[derive(Debug)]
pub struct Limited {
    pub stamp_100ns: i64,
    /// The limited block(s); `None` = the standby silence.
    pub audio: Option<Vec<AudioFrame>>,
    /// When the block was handed to the outputs (timeline, 100 ns).
    pub fed_100ns: i64,
    /// Frames the limiter scaled in it (the fade's INFO line).
    pub limited_frames: u64,
}

/// The program's audio side: the crossfade of a window boundary, the ONE
/// peak limiter (#210), the hand-off to every output (#233).
pub struct AudioSide {
    /// The silent block the limiter runs over for a standby boundary.
    silence: Vec<AudioFrame>,
    limiter: PeakLimiter,
    /// The boundary the limiter last ran on. A stamp that is not the grid
    /// boundary right after it restarts the program's timeline.
    limited_through: Option<i64>,
    outputs: Option<Arc<AudioOutputs>>,
}

impl Default for AudioSide {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioSide {
    /// An audio side that feeds no output yet.
    pub fn new() -> Self {
        Self {
            silence: silent_block(),
            limiter: PeakLimiter::new(PROGRAM_AUDIO_RATE_HZ),
            limited_through: None,
            outputs: None,
        }
    }

    /// Also hand every block to `outputs`.
    pub fn with_outputs(mut self, outputs: Arc<AudioOutputs>) -> Self {
        self.outputs = Some(outputs);
        self
    }

    /// The outputs it feeds.
    pub fn outputs(&self) -> Option<&Arc<AudioOutputs>> {
        self.outputs.as_ref()
    }

    /// Make `job`'s block (a fade's crossfade), limit it, hand it to every
    /// output; the instant of the hand-off is read off `now`.
    pub fn process(&mut self, job: AudioJob, now: &impl Fn() -> i64) -> Limited {
        let stamp_100ns = job.stamp_100ns();
        let fade = matches!(job, AudioJob::Mix { .. });
        let mut audio = match job {
            AudioJob::Source { audio, .. } => Some(audio),
            AudioJob::Standby { .. } => None,
            AudioJob::Mix {
                from,
                to,
                first,
                total,
                ..
            } => {
                let format = AudioFormat {
                    frames: program_spc(),
                    channels: PROGRAM_AUDIO_CHANNELS,
                    sample_rate: PROGRAM_AUDIO_RATE_HZ,
                };
                let mixed = mix_audio_block(from.as_ref(), to.as_ref(), first, total, format);
                Some(vec![mixed])
            }
        };
        let limited_frames = self.limit(audio.as_deref_mut(), stamp_100ns, fade);
        let block = match &audio {
            Some(frames) => ProgramBlock::copied(stamp_100ns, frames),
            None => ProgramBlock::silence(stamp_100ns),
        };
        if let Some(outputs) = &self.outputs {
            outputs.offer(&block);
        }
        Limited {
            stamp_100ns,
            audio,
            fed_100ns: now(),
            limited_frames,
        }
    }

    /// #210: run the boundary's audio through the limiter, in place (`None`:
    /// the standby silence: 0 × gain stays 0, and its tail decays in step
    /// with time). A stamp that is not the grid boundary right after the
    /// last one restarts the program's timeline: the release tail is
    /// dropped. A block that is not one program block passes as it came.
    /// #257: an over outside a fade is the ledger's risk. Returns the frames
    /// it scaled.
    fn limit(&mut self, audio: Option<&mut [AudioFrame]>, stamp_100ns: i64, fade: bool) -> u64 {
        let follows = self
            .limited_through
            .is_some_and(|last| strict_next_boundary_100ns(last, GENLOCK_GRID_FPS) == stamp_100ns);
        if !follows {
            self.limiter.reset();
        }
        self.limited_through = Some(stamp_100ns);
        let before = self.limiter.limited_frames();
        let overs = self.limiter.over_frames();
        let frames: &mut [AudioFrame] = match audio {
            Some(frames) => frames,
            None => &mut self.silence,
        };
        for frame in frames {
            if is_program_block(frame) {
                let channels = frame.channels as usize;
                self.limiter.process(&mut frame.data, channels);
            }
        }
        if !fade && let Some(outputs) = &self.outputs {
            outputs.note_limiter_overs(self.limiter.over_frames() - overs);
        }
        self.limiter.limited_frames() - before
    }
}

/// The line's counters (`health.audio.line`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct LineStatus {
    /// Blocks the sender passed over (its job for them was coalesced).
    pub stale: u64,
    /// Boundaries the sender found no block for: NDI got the silence.
    pub missing: u64,
    /// Blocks dropped over [`LINE_BOUND`].
    pub dropped: u64,
}

/// What the line had for a stamp.
#[derive(Debug)]
pub enum LineTake {
    Block(Limited),
    /// A later block is first: this stamp's never came (its audio job was
    /// coalesced).
    Missing,
    /// Nothing yet.
    NotYet,
}

/// The line's state (pure).
#[derive(Debug, Default)]
pub struct LineState {
    blocks: VecDeque<Limited>,
    status: LineStatus,
}

impl LineState {
    /// Keep a limited block, the oldest dropped over [`LINE_BOUND`].
    pub fn push(&mut self, block: Limited) {
        if self.blocks.len() >= LINE_BOUND {
            self.blocks.pop_front();
            self.status.dropped += 1;
        }
        self.blocks.push_back(block);
    }

    /// The block of `stamp_100ns`; every older one is passed over (stale).
    pub fn take(&mut self, stamp_100ns: i64) -> LineTake {
        while self
            .blocks
            .front()
            .is_some_and(|b| b.stamp_100ns < stamp_100ns)
        {
            self.blocks.pop_front();
            self.status.stale += 1;
        }
        match self.blocks.front() {
            Some(b) if b.stamp_100ns == stamp_100ns => {
                LineTake::Block(self.blocks.pop_front().expect("the front was just read"))
            }
            Some(_) => LineTake::Missing,
            None => LineTake::NotYet,
        }
    }

    /// The blocks kept (the tests' view).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn status(&self) -> LineStatus {
        self.status
    }
}

/// The stamp-keyed line of limited blocks between the audio thread and the
/// `SP-program` sender.
#[derive(Default)]
pub struct AudioLine {
    state: Mutex<LineState>,
    ready: Condvar,
}

impl AudioLine {
    fn lock(&self) -> std::sync::MutexGuard<'_, LineState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The audio thread: keep `block` for the sender.
    pub fn push(&self, block: Limited) {
        self.lock().push(block);
        self.ready.notify_all();
    }

    /// The sender: the block of `stamp_100ns`, waiting at most `wait` for
    /// it; `None` (counted `missing`) when it never came.
    pub fn take(&self, stamp_100ns: i64, wait: Duration) -> Option<Limited> {
        let deadline = Instant::now() + wait;
        let mut st = self.lock();
        loop {
            match st.take(stamp_100ns) {
                LineTake::Block(block) => return Some(block),
                LineTake::Missing => break,
                LineTake::NotYet => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        break;
                    }
                    st = self
                        .ready
                        .wait_timeout(st, left)
                        .unwrap_or_else(|p| p.into_inner())
                        .0;
                }
            }
        }
        st.status.missing += 1;
        None
    }

    pub fn status(&self) -> LineStatus {
        self.lock().status()
    }
}

/// How long the sender holds a job stamped `stamp_100ns` at `now_100ns`
/// (100 ns): until its stamp when it lies ahead within [`HOLD_MAX_100NS`],
/// else not at all.
pub fn hold_100ns(stamp_100ns: i64, now_100ns: i64) -> i64 {
    let ahead = stamp_100ns - now_100ns;
    if ahead > 0 && ahead <= HOLD_MAX_100NS {
        ahead
    } else {
        0
    }
}

/// The sender holds a job taken at `taken_100ns` to its stamp on `clock`
/// (production: `SpinClock`); returns the instant it serves it (`now` after
/// a hold, `taken_100ns` without one).
pub fn hold_to_stamp(
    clock: &mut dyn SendClock,
    stamp_100ns: i64,
    taken_100ns: i64,
    now: &impl Fn() -> i64,
) -> i64 {
    let hold = hold_100ns(stamp_100ns, taken_100ns);
    if hold == 0 {
        return taken_100ns;
    }
    let due = clock.now() + Duration::from_nanos(hold as u64 * 100);
    clock.wait_until(due);
    now()
}

/// The smallest margin of a window (µs: stamp − hand-off), `None` before any.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MinMargin(pub Option<i64>);

impl Worst for MinMargin {
    fn worst(self, other: Self) -> Self {
        match (self.0, other.0) {
            (Some(a), Some(b)) => MinMargin(Some(a.min(b))),
            (a, b) => MinMargin(a.or(b)),
        }
    }
}

/// `GET /api/v1/program` → `health.audio`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct AudioAheadStatus {
    /// The slots the sources commit ahead (0: the audio side is inline).
    pub lead_slots: i64,
    /// Boundaries whose audio reached the outputs before their stamp.
    pub boundaries_ahead: u64,
    /// The smallest stamp − hand-off over the last 60–120 s (µs, negative =
    /// after the stamp); `None` before any boundary.
    pub margin_us_min: Option<i64>,
    /// Audio jobs dropped because the audio thread fell a queue behind.
    pub coalesced: u64,
    pub line: LineStatus,
}

/// The margin telemetry (pure).
#[derive(Debug, Default)]
pub struct MarginWindow {
    ahead: u64,
    window: TwoBucketWorst<MinMargin, TIMING_BUCKET_BOUNDARIES>,
}

impl MarginWindow {
    /// One boundary stamped `stamp_100ns`, handed to the outputs at
    /// `fed_100ns`.
    pub fn observe(&mut self, stamp_100ns: i64, fed_100ns: i64) {
        let margin_100ns = stamp_100ns - fed_100ns;
        self.ahead += u64::from(margin_100ns > 0);
        self.window.push(MinMargin(Some(margin_100ns / 10)));
    }

    pub fn ahead(&self) -> u64 {
        self.ahead
    }

    pub fn margin_us_min(&self) -> Option<i64> {
        self.window.worst().0
    }
}

/// The `program-audio` thread's loop: every audio job the bus queues, in
/// order, through `side` into the bus's line; exits once the bus is stopped
/// and its audio queue is drained.
pub fn run_audio_loop(side: &mut AudioSide, bus: &ProgramBus, now: impl Fn() -> i64) {
    use crate::playback::program_bus::AudioTake;
    loop {
        match bus.take_audio_timeout(AUDIO_IDLE_WAIT) {
            AudioTake::Job(job) => {
                let limited = side.process(job, &now);
                bus.audio_line().push(limited);
            }
            AudioTake::Idle => {}
            AudioTake::Stopped => break,
        }
    }
    tracing::info!("program audio: stopped");
}

/// Windows: the `program-audio` thread (MMCSS "Pro Audio", like the VBAN
/// threads), feeding the bus's outputs. Glue: the loop is
/// [`run_audio_loop`], tested on its own.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
pub fn spawn_audio_thread(bus: Arc<ProgramBus>) {
    let spawned = std::thread::Builder::new()
        .name("program-audio".into())
        .spawn(move || {
            let _mmcss = crate::playback::mmcss::join_pro_audio("program-audio");
            let mut side = AudioSide::new().with_outputs(bus.outputs().clone());
            tracing::info!("program audio thread started (#289 step 2)");
            run_audio_loop(
                &mut side,
                &bus,
                crate::playback::fleet_shift::timeline_now_100ns,
            );
        });
    if let Err(e) = spawned {
        tracing::error!(%e, "failed to spawn the program audio thread — no audio reaches the outputs");
    }
}

#[cfg(test)]
#[path = "program_audio_tests.rs"]
mod tests;
