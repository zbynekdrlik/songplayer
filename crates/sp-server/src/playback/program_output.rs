//! The `SP-program` NDI output (#209, B1 of EPIC #174).
//!
//! [`ProgramOutput`] owns the program's own paced [`FrameSubmitter`] (NDI
//! sender [`PROGRAM_NDI_NAME`], `clock_video = false` like every paced output)
//! and submits each [`ProgramJob`] the [`ProgramBus`] queued: a forwarded source
//! boundary (the same audio block, the same stamps, and its picture as a
//! 1920×1080 canvas picture, #223 below: the same `Arc` frame when it already
//! is one), or the program's own #147 standby pair (the cached NV12 black + one
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
//!
//! #210: every submitted pair's audio block (forwarded, mixed, or the standby
//! silence) is handed to the program's VBAN output (`vban_out.rs`) BEFORE its
//! NDI submit — a copy, the NDI submit still borrows the pair — so FOH audio
//! never waits for the video side of its own boundary (a slow NDI send, a
//! mixed picture; a video side longer than a slot still delays the NEXT
//! boundary's take, which `health.timing` shows as `ready_late_us`), and
//! `start_program` also starts the VBAN thread + its settings task. Every
//! block first goes through the program's ONE peak limiter (#210, after the
//! crossfade, finding 5986249387): the same `sp_decoder::PeakLimiter` the
//! stem mix uses, its state carried from one boundary to the next, reset
//! where the program's timeline restarts. A fade sums two sources that are
//! each at most 0.98 up to 0.98·√2 at mid-fade, which VBAN's INT24 encoder
//! would clamp flat at FOH. SongPlayer's own playlists are at or under the
//! ceiling, so once a fade's release tail has decayed (≤ ~24 boundaries)
//! the limiter is at rest and their blocks pass bit for bit; a hotter block
//! (the NDI input "OBS manuál" forwards cg OBS's audio as it comes) is
//! limited too, where VBAN used to clamp it.
//!
//! #215: a [`ProgramJob::Mix`] (one boundary inside a transition window) is
//! crossfaded here, on the sender thread: the audio per sample with the
//! equal-power curve, the picture blended into a `frame_pool` buffer, painted
//! in row bands (`program_transition::mix_nv12_into`, #215 addendum 3) on the
//! sender's persistent band workers (`band_pool.rs`, #223 follow-up: started
//! once with the output, never per picture). `start_program` also starts the
//! OBS-follow task (`program_follow.rs`) and hands the bus to the engine for
//! the deferred scene-go-off pause.
//!
//! #223: `SP-program` is ALWAYS 1920×1080 (the owner's rule). Every picture
//! the sender submits is in its canvas (`program_canvas.rs`, the standby's own
//! 1920×1080 in production): a forwarded source's (the NDI input's included)
//! is passed through when it already is one, else fitted into it; a fade's
//! picture is painted in ONE pass from both sides (#223 follow-up, design
//! record 5973498519), each fitted into the canvas as it is read when it is
//! not a canvas picture, the incoming one blended over the outgoing one; a
//! missing side is the canvas black. The fit is video-side work in
//! `submit_video`, after the VBAN hand-off, and inside the `submit_us` span
//! (`health.timing`), so the box shows its cost.

use std::sync::Arc;
use std::time::{Duration, Instant};

use sp_core::genlock::audio::samples_per_boundary;
use sp_core::genlock::{
    GENLOCK_GRID_FPS, GENLOCK_MAX_CATCHUP_INTERVALS, floor_boundary_100ns, lag_slots_100ns,
    strict_next_boundary_100ns,
};
use sp_decoder::PeakLimiter;
use sp_ndi::{AudioFrame, NdiBackend, NdiSender};
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::playback::band_pool::BandPool;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_bus::{
    PROGRAM_NDI_NAME, ProgramBus, ProgramJob, Take, install, restore_selected_source,
};
use crate::playback::program_canvas::{Canvas, FadeSide};
use crate::playback::program_output_timing::{BoundaryMarks, LateBoundary, utc_label};
use crate::playback::program_transition::{
    AudioFormat, Layout, MIX_THREAD_NAME, MixJob, mix_audio_block, mix_bands,
};
use crate::playback::submit_handoff::SubmitJob;
use crate::playback::submitter::FrameSubmitter;
use crate::playback::vban_out::{VbanBlock, VbanOut, is_program_block, run_vban_config_task};
use crate::playback::wallclock::WallClock;

/// The program's picture size, 1080p (the paced idle size): #223, every
/// picture `SP-program` sends is this canvas, its standby black included
/// (`program_canvas.rs`). The NDI input's standby black is this size too.
pub const PROGRAM_STANDBY_W: u32 = 1920;
pub const PROGRAM_STANDBY_H: u32 = 1080;

/// The program's silent block: 48 kHz stereo, one grid slot.
const PROGRAM_AUDIO_RATE_HZ: u32 = 48_000;
const PROGRAM_AUDIO_CHANNELS: u32 = 2;

// #210: the VBAN output carries exactly the program's audio format.
const _: () = assert!(
    PROGRAM_AUDIO_RATE_HZ as i64 == crate::playback::vban_packet::VBAN_SAMPLE_RATE_HZ
        && PROGRAM_AUDIO_CHANNELS as usize == crate::playback::vban_packet::VBAN_CHANNELS
);

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
    /// #223: the program's ONE picture layout, every picture it sends (the
    /// standby black included), with the fit plans into it.
    canvas: Canvas,
    /// #210: the VBAN output each submitted pair's audio block goes to.
    vban: Option<Arc<VbanOut>>,
    /// #215: audio frames per boundary (1600).
    spc: usize,
    /// #215 addendum 3 + #223 follow-up: the row bands a mixed or fitted
    /// picture is painted in (`mix_bands` of the box's logical processors):
    /// this thread and the persistent `program-mix-<i>` workers, started
    /// with the output and joined when it is dropped.
    bands: BandPool,
    /// The run of mixed boundaries being sent (a window), logged once when
    /// the next unmixed boundary ends it.
    mix_run: MixRun,
    /// #210: the program's peak limiter. Every program block goes through it
    /// before VBAN and the NDI submit get it ([`limit`](Self::limit)).
    limiter: PeakLimiter,
    /// #210: the boundary the limiter last ran on. A stamp that is not the
    /// grid boundary right after it is a restart of the program's timeline.
    limited_through: Option<i64>,
}

/// #215: one run of mixed boundaries as the `SP-program` sender saw it: how
/// many, how many had a side fitted into the canvas (#223: a side not already
/// a canvas picture), and the worst time the picture (the fits + blend, all
/// their row bands) took, measured on this thread — the cost the review asked
/// to see on the box, next to `health.coalesced`. #210: also how many frames
/// the program's peak limiter scaled while the run went out: its mixed
/// boundaries and the boundary that ended it (where the release tail
/// starts), so the box shows the limiter working through a fade.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct MixRun {
    pub(crate) boundaries: u64,
    pub(crate) fitted: u64,
    pub(crate) max_picture_us: u64,
    pub(crate) limited: u64,
}

/// The one INFO line of a finished run of mixed boundaries (none for an
/// empty run). Logging only.
#[cfg_attr(test, mutants::skip)]
fn log_mix_run(run: &MixRun) {
    if run.boundaries > 0 {
        info!(
            boundaries = run.boundaries,
            fitted = run.fitted,
            max_picture_us = run.max_picture_us,
            limited_frames = run.limited,
            "program transition: the fade's mixed boundaries went out"
        );
    }
}

/// #215: whether a window boundary has a picture to mix: either side is
/// here. A mix with neither side (the bus never queues one) is the standby
/// pair.
fn has_picture(mix: &MixJob) -> bool {
    mix.from.is_some() || mix.to.is_some()
}

/// #223 follow-up: a window boundary's side as `Canvas::fade` reads it: its
/// picture's layout and bytes, or `None` when the side is missing.
fn fade_side(job: &Option<SubmitJob>) -> FadeSide<'_> {
    job.as_ref().map(|job| (Layout::of(job), &job.video[..]))
}

/// #210: one program boundary split at the VBAN hand-off
/// ([`ProgramOutput::split`]): its audio is decided, its video work is
/// still to do ([`ProgramOutput::submit_video`]).
enum Pair {
    /// A forwarded source pair, as it came.
    Source(SubmitJob),
    /// The program's own standby pair (the black + one silent block).
    Standby,
    /// #215: a window boundary: its crossfaded block, and the mix whose
    /// picture is painted in the canvas.
    Mix { mix: MixJob, audio: Vec<AudioFrame> },
}

/// Submit one pair on the program's sender: `video`, a `canvas` picture,
/// stamped on `stamp_100ns`, and `audio` on `audio_tc_100ns`.
fn send<B: NdiBackend>(
    submitter: &mut FrameSubmitter<B>,
    canvas: Layout,
    video: SharedFrame,
    audio: &[AudioFrame],
    stamp_100ns: i64,
    audio_tc_100ns: i64,
) {
    submitter.submit_frame_at_boundary_owned(
        canvas.width,
        canvas.height,
        canvas.stride,
        video,
        audio,
        stamp_100ns,
        audio_tc_100ns,
    );
}

impl Pair {
    /// #210: the audio this pair carries to VBAN and NDI: a forwarded
    /// pair's own, a fade's crossfaded block, or the program's one silent
    /// block for a standby pair (`silence`).
    fn audio_mut<'a>(&'a mut self, silence: &'a mut [AudioFrame]) -> &'a mut [AudioFrame] {
        match self {
            Pair::Source(job) => &mut job.audio,
            Pair::Standby => silence,
            Pair::Mix { audio, .. } => audio,
        }
    }

    /// What VBAN gets for the boundary on `stamp_100ns`: the pair's own
    /// block, COPIED (its NDI submit still borrows it), or the standby
    /// silence.
    fn vban_block(&self, stamp_100ns: i64) -> VbanBlock {
        match self {
            Pair::Source(job) => VbanBlock::copied(stamp_100ns, &job.audio),
            Pair::Standby => VbanBlock::silence(stamp_100ns),
            Pair::Mix { audio, .. } => VbanBlock::copied(stamp_100ns, audio),
        }
    }
}

impl<B: NdiBackend> ProgramOutput<B> {
    /// Wrap the program's NDI sender. #223: every picture it sends, the
    /// standby black included, is a `width`×`height` NV12 canvas picture,
    /// stride `width` ([`PROGRAM_STANDBY_W`] × [`PROGRAM_STANDBY_H`] =
    /// 1920×1080 in production; the tests use small canvases).
    pub fn new(sender: NdiSender<B>, width: u32, height: u32) -> Self {
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
            canvas: Canvas::new(width, height),
            vban: None,
            spc,
            bands: BandPool::new(
                MIX_THREAD_NAME,
                mix_bands(crate::lyrics::heavy_slot::logical_cores()),
            ),
            mix_run: MixRun::default(),
            limiter: PeakLimiter::new(PROGRAM_AUDIO_RATE_HZ),
            limited_through: None,
        }
    }

    /// The `SP-program` output as production builds it (#223): the 1920×1080
    /// canvas ([`PROGRAM_STANDBY_W`] × [`PROGRAM_STANDBY_H`]).
    pub fn fhd(sender: NdiSender<B>) -> Self {
        Self::new(sender, PROGRAM_STANDBY_W, PROGRAM_STANDBY_H)
    }

    /// #210: also hand every submitted pair's audio block to `vban`.
    pub fn with_vban(mut self, vban: Arc<VbanOut>) -> Self {
        self.vban = Some(vban);
        self
    }

    /// #210: hand one pair's audio block to the VBAN output (never blocks);
    /// returns the instant it was handed over, read off `now`.
    fn feed_vban(&self, block: VbanBlock, now: &impl Fn() -> i64) -> i64 {
        if let Some(vban) = &self.vban {
            vban.push(block);
        }
        now()
    }

    /// Serve one program boundary: its audio side ([`split`](Self::split),
    /// no video work) goes to VBAN FIRST, then the video side and the NDI
    /// pair ([`submit_video`](Self::submit_video)) — #210: FOH audio never
    /// waits for the video side of its own boundary (the NDI submit, a mixed
    /// picture). A forwarded source job keeps its own stamps. What the
    /// program makes itself (a standby pair, a mixed block) is stamped on
    /// its boundary, the audio too: the block belongs to that boundary's
    /// timeline instant, never the submit instant — a standby pair for a
    /// missed boundary goes out up to the fill grace (3 slots) late (#224).
    /// An unmixed boundary ends the run of mixed boundaries once it went
    /// out. Returns the instants the boundary was served at, read off `now`
    /// (the sender's wall: the stamps' timeline), for `health.timing`
    /// (`program_output_timing.rs`).
    pub fn serve(&mut self, job: ProgramJob, now: impl Fn() -> i64) -> BoundaryMarks {
        let taken_100ns = now();
        let stamp_100ns = job.stamp_100ns();
        let mut pair = self.split(job);
        self.limit(&mut pair, stamp_100ns);
        let fed_100ns = self.feed_vban(pair.vban_block(stamp_100ns), &now);
        let ends_run = !matches!(pair, Pair::Mix { .. });
        let submit_start_100ns = self.submit_video(pair, stamp_100ns, &now);
        let submitted_100ns = now();
        if ends_run {
            self.end_mix_run();
        }
        BoundaryMarks {
            stamp_100ns,
            taken_100ns,
            fed_100ns,
            submit_start_100ns,
            submitted_100ns,
        }
    }

    /// The tests' shorthand: [`serve`](Self::serve) with no clock; the stamp.
    #[cfg(test)]
    pub fn submit(&mut self, job: ProgramJob) -> i64 {
        self.serve(job, || 0).stamp_100ns
    }

    /// #210: the audio side of `job`, with no video work: a window
    /// boundary's crossfaded block is computed here (#215; a mix with
    /// neither side — the bus never queues one — is the standby pair).
    fn split(&self, job: ProgramJob) -> Pair {
        match job {
            ProgramJob::Source(job) => Pair::Source(job),
            ProgramJob::Standby { .. } => Pair::Standby,
            ProgramJob::Mix(mix) if !has_picture(&mix) => Pair::Standby,
            ProgramJob::Mix(mix) => {
                let (first, total) = mix.sample_span(self.spc);
                let format = AudioFormat {
                    frames: self.spc,
                    channels: PROGRAM_AUDIO_CHANNELS,
                    sample_rate: PROGRAM_AUDIO_RATE_HZ,
                };
                let audio = vec![mix_audio_block(
                    mix.from.as_ref().and_then(|j| j.audio.first()),
                    mix.to.as_ref().and_then(|j| j.audio.first()),
                    first,
                    total,
                    format,
                )];
                Pair::Mix { mix, audio }
            }
        }
    }

    /// #210: run the boundary's audio through the program's peak limiter,
    /// in place, BEFORE VBAN's copy and the NDI submit: both then carry the
    /// same limited block, VBAN first. Every program block goes through it
    /// (a forwarded pair's, a fade's crossfaded block, the standby silence:
    /// 0 × gain stays 0, and its tail decays in step with time), so its
    /// state, and the gain, carry from one boundary to the next. A stamp that
    /// is not the grid boundary right after the last one (the first boundary,
    /// a resync that skipped stamps) restarts the program's timeline: the
    /// audio after it is unrelated, so the release tail is dropped. A block
    /// that is not one program block (48 kHz stereo, one boundary: what VBAN
    /// carries; VBAN sends silence for anything else) passes as it came.
    fn limit(&mut self, pair: &mut Pair, stamp_100ns: i64) {
        let follows = self
            .limited_through
            .is_some_and(|last| strict_next_boundary_100ns(last, GENLOCK_GRID_FPS) == stamp_100ns);
        if !follows {
            self.limiter.reset();
        }
        self.limited_through = Some(stamp_100ns);
        let before = self.limiter.limited_frames();
        for frame in pair.audio_mut(&mut self.silence) {
            if is_program_block(frame) {
                let channels = frame.channels as usize;
                self.limiter.process(&mut frame.data, channels);
            }
        }
        self.mix_run.limited += self.limiter.limited_frames() - before;
    }

    /// #210: the video side of a boundary VBAN already has, then its NDI
    /// pair: a forwarded pair's picture made a canvas picture (#223: as it is
    /// when it already is one, else fitted into the canvas), the standby
    /// black + silence, or a window boundary's mixed picture (#215) + its
    /// crossfaded block, both stamped on the window boundary (#224: the
    /// program's own block). Returns when the video side started, read off
    /// `now` BEFORE the picture is made (#223), so `health.timing.submit_us`
    /// is the fit or the fade's picture plus the NDI submit.
    fn submit_video(&mut self, pair: Pair, stamp_100ns: i64, now: &impl Fn() -> i64) -> i64 {
        let start = now();
        let canvas = self.canvas.layout();
        match pair {
            Pair::Source(job) => {
                let layout = Layout::of(&job);
                let video = self.canvas.fit(layout, &job.video, &self.bands);
                let audio_tc = job.audio_tc_100ns;
                send(
                    &mut self.submitter,
                    canvas,
                    video,
                    &job.audio,
                    stamp_100ns,
                    audio_tc,
                );
            }
            Pair::Standby => {
                let black = self.canvas_black();
                send(
                    &mut self.submitter,
                    canvas,
                    black,
                    &self.silence,
                    stamp_100ns,
                    stamp_100ns,
                );
            }
            Pair::Mix { mix, audio } => {
                let started = Instant::now();
                let video = self.paint_mix(&mix);
                let picture_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
                self.mix_run.boundaries += 1;
                self.mix_run.max_picture_us = self.mix_run.max_picture_us.max(picture_us);
                send(
                    &mut self.submitter,
                    canvas,
                    video,
                    &audio,
                    stamp_100ns,
                    stamp_100ns,
                );
            }
        }
        start
    }

    /// #223: the canvas's studio black — the standby picture
    /// (`FrameSubmitter::standby_black_nv12`: built once, then an `Arc`
    /// clone).
    fn canvas_black(&mut self) -> SharedFrame {
        let canvas = self.canvas.layout();
        self.submitter
            .standby_black_nv12(canvas.width, canvas.height)
    }

    /// #215: the picture of a window boundary ([`paint_mix`](Self::paint_mix))
    /// in the canvas layout, `None` when neither side is here — the tests'
    /// view of it.
    #[cfg(test)]
    pub(crate) fn mix_picture(&mut self, mix: &MixJob) -> Option<(Layout, SharedFrame)> {
        if !has_picture(mix) {
            return None;
        }
        Some((self.canvas.layout(), self.paint_mix(mix)))
    }

    /// #223: how many canvas fit plans the sender built so far — the tests'
    /// view of `Canvas::built`.
    #[cfg(test)]
    pub(crate) fn fit_plans(&self) -> u64 {
        self.canvas.built()
    }

    /// #215 + #223: the picture of a window boundary, in the canvas, exactly
    /// its bytes, painted in ONE pass (`Canvas::fade`, #223 follow-up): the
    /// incoming picture blended over the outgoing one at the boundary's
    /// weight, each one fitted into the canvas as it is read when it is not a
    /// canvas picture (no fitted scratch, no second pass), a missing side the
    /// canvas's studio black, in the sender's row bands into a pooled buffer.
    fn paint_mix(&mut self, mix: &MixJob) -> SharedFrame {
        let (from, to) = (fade_side(&mix.from), fade_side(&mix.to));
        let fitted = [from, to]
            .into_iter()
            .flatten()
            .any(|(layout, _)| !self.canvas.holds(layout));
        let video = self.canvas.fade(from, to, mix.weight_q8(), &self.bands);
        self.mix_run.fitted += u64::from(fitted);
        video
    }

    /// End a run of mixed boundaries: log it once and start the next.
    fn end_mix_run(&mut self) {
        log_mix_run(&std::mem::take(&mut self.mix_run));
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

/// #210: the WARN of a boundary whose VBAN hand-off came after its block's
/// first packet was due (over VBAN's send latency L, part 2): its three
/// figures, the boundary as its wire stamp and in UTC (to line up with a
/// dev1 capture), and how many such boundaries the rate limit skipped since
/// the WARN before it. The decision is `BoundaryTiming::observe`'s (tested);
/// logging only.
#[cfg_attr(test, mutants::skip)]
fn warn_late_boundary(late: &LateBoundary) {
    let wire = crate::playback::fleet_shift::wire_100ns(late.stamp_100ns);
    warn!(
        boundary_100ns = wire,
        boundary_utc = %utc_label(wire),
        ready_late_us = late.sample.ready_late_us,
        vban_feed_late_us = late.sample.vban_feed_late_us,
        submit_us = late.sample.submit_us,
        suppressed = late.suppressed,
        "program output: a boundary's VBAN audio was handed over after its first packet was due (over VBAN's send latency) — its packets go out late, back to back"
    );
}

/// Most wall ticks one wake may owe: the pacer's own catch-up bound, beyond
/// which a pacer resyncs without ticking — so a long stall costs the program
/// wall no more re-anchor progress than it costs the stamp walls.
pub const MAX_TICKS_PER_WAKE: i64 = GENLOCK_MAX_CATCHUP_INTERVALS;

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
/// (after a flush) once the bus is stopped and drained. #210: every boundary's
/// timing goes to the bus (`health.timing`), and a VBAN hand-off after its
/// block's first packet was due is WARNed (rate-limited by
/// `BoundaryTiming::observe`).
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
                let marks = out.serve(job, || wall.now_100ns());
                bus.record_submitted(marks.stamp_100ns);
                if let Some(late) = bus.record_timing(&marks) {
                    warn_late_boundary(&late);
                }
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
    /// #210: also start the VBAN thread (Windows) and its settings task.
    /// #212: also start the NDI input "OBS manuál" (its settings task, and on
    /// Windows its grid thread on the engine's NDI SDK). #213: also start the
    /// Companion remote control's settings task (its listener cuts this bus and
    /// reaches cg OBS through the engine's OBS client). #215: also start the
    /// OBS-follow task and keep the bus for the deferred scene-go-off pause;
    /// #219: the follow consumes the OBS client's snapshots (`obs`).
    /// #221 L4b: also tell cg OBS once what the restored program shows (the
    /// startup re-mirror) and start the playback authority
    /// (`program_authority.rs`), whose first value plays the restored program.
    /// Call once, after the #196 startup senders.
    #[cfg_attr(test, mutants::skip)]
    pub async fn start_program(
        &self,
        bus: Arc<ProgramBus>,
        shutdown: &broadcast::Sender<()>,
        obs: tokio::sync::watch::Receiver<crate::obs::ObsSnapshot>,
    ) {
        let _ = self.program.set(bus.clone()); // #215: the deferred scene-go-off pause
        let vban = bus.vban().clone();
        tokio::spawn(run_vban_config_task(
            self.pool.clone(),
            vban.clone(),
            shutdown.subscribe(),
        ));
        #[cfg(windows)]
        crate::playback::vban_out::spawn_vban_thread(vban.clone());
        let mut shutdown_rx = shutdown.subscribe();
        restore_selected_source(&self.pool, &bus).await;
        if !install(bus.clone()) {
            warn!("program bus: a bus was already installed — keeping the first one");
        }
        #[cfg(windows)]
        spawn_program_thread(self.ndi_backend.clone(), bus.clone());
        #[cfg(windows)]
        let receive = self
            .ndi_backend
            .as_ref()
            .and_then(|b| sp_ndi::RealNdiReceiveBackend::new(b.lib().clone()))
            .map(|r| Arc::new(r) as Arc<dyn sp_ndi::NdiReceiveBackend>);
        #[cfg(not(windows))]
        let receive: Option<Arc<dyn sp_ndi::NdiReceiveBackend>> = None;
        crate::playback::ndi_input::start_ndi_input(
            self.pool.clone(),
            bus.clone(),
            receive,
            shutdown,
        );
        let upstream =
            crate::remote::Upstream::new(self.obs_cmd_tx.clone(), self.obs_event_tx.clone());
        // #221 L4a: the dashboard's cut mirrors to cg OBS through the same link.
        bus.legacy_cg().attach(upstream.clone());
        // #221 L4b decision 1: cg OBS is told once what the restored program shows.
        crate::playback::program_switch::remirror_on_air(&bus, &upstream).await;
        // #221 L4b: SP-program (∪ SongPlayer's cg OBS record) drives playback.
        tokio::spawn(super::program_authority::run_program_authority(
            bus.clone(),
            self.event_tx.clone(),
            self.on_air.clone(),
            shutdown.subscribe(),
        ));
        let follow = crate::playback::program_follow::Follow::new(self.pool.clone(), bus.clone());
        crate::playback::program_follow::start_follow(follow, obs, shutdown);
        crate::remote::start_remote(self.pool.clone(), bus.clone(), upstream, shutdown);
        tokio::spawn(async move {
            let _ = shutdown_rx.recv().await;
            bus.stop();
            vban.stop();
            bus.input().stop();
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
            let mut out = ProgramOutput::fhd(sender).with_vban(bus.vban().clone());
            // #215 addendum 3 + #223 follow-up: how many threads paint a mixed
            // or fitted picture (this one + the persistent band workers).
            info!(
                ndi_name = PROGRAM_NDI_NAME,
                width = PROGRAM_STANDBY_W,
                height = PROGRAM_STANDBY_H,
                mix_bands = out.bands.bands(),
                mix_workers = out.bands.workers(),
                "program output thread started (#223: every picture is this canvas)"
            );
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
#[cfg(test)]
#[path = "program_output_tests_fhd.rs"]
mod tests_fhd;
#[cfg(test)]
#[path = "program_output_tests_limit.rs"]
mod tests_limit;
#[cfg(test)]
#[path = "program_output_tests_order.rs"]
mod tests_order;
