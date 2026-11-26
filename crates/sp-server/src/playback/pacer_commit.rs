//! The pacer commits ahead (#289 step 1, design comment 6814927800).
//!
//! The program's audio is to be handed to the outputs one slot BEFORE its
//! boundary (step 2). So the pacer now DECIDES boundary B + K at boundary B
//! (`AUDIO_LEAD_SLOTS` K = 1): its decision clock ([`Pacer::now_100ns`])
//! reads the wall plus K slots. Every decision (the due frame, drop / park,
//! the blend toward the parked next frame, the audio block taken from the
//! grid with `correction_for`, catch-up, resync, re-anchor, the lateness
//! telemetry) is made exactly as before, one slot earlier, so every
//! boundary's (picture, audio) pair is the one the pacer gave before.
//!
//! The decision path sinks into a [`CommitLine`] (a FIFO of the recorded
//! [`PacedSink`] calls). A pair leaves the line, through the SAME sink method
//! and with the same picture, block and stamps, once the REAL wall reaches
//! its stamp: what an output receives, and when, is unchanged.
//!
//! - A command read after a commit (pause, resume, seek, stop, a new Play,
//!   the song's end) reaches the pacer at its next decision, so it applies
//!   from the first boundary not yet committed; the committed pair still
//!   leaves.
//! - A scope end (a song, an idle stretch) ends by `PacedFeed::finish`:
//!   [`Pacer::flush_line`], then the detach, so the paced output continues
//!   after the last committed stamp.
//! - The song's `Started` waits for the wall one slot before its first
//!   boundary ([`Pacer::wait_one_slot_before_next`]) and a `Position` reads
//!   [`Pacer::shown_position_ms`], so the engine's title clock and lyrics
//!   keep their relation to the picture.
//! - A pair whose stamp lies more than the lead + one slot ahead of the real
//!   wall (the wall stepped back after the commit) leaves at once, in order,
//!   as the pacer would have emitted it at its decision.
//!
//! With K = 0 (`Pacer::with_wallclock`, the unit tests' constructor) every
//! pair leaves in the call that decided it: the path before step 1.
//!
//! #289 step 2: the production pacer ([`Pacer::new`]) also HANDS each pair
//! out at its commit, one slot before its stamp ([`Pacer::handing_ahead`]):
//! the bus decides it then, its audio reaches the outputs a slot early, and
//! the `SP-program` sender holds the picture to its stamp
//! (`program_audio.rs`). The pairs are the same; only when they leave the
//! pacer moves. [`Pacer::with_lead`] keeps step 1's release at the stamp.
//!
//! A child of `pacer.rs` (1000-line cap), like `pacer_prepare.rs`.

use std::collections::VecDeque;

use sp_ndi::AudioFrame;

use super::{FrameBlend, PacedFrame, PacedSink, Pacer, ServiceOutcome, Standby};
use crate::playback::picture::Picture;
use crate::playback::program_bus::CUT_LEAD_SLOTS;
use crate::playback::wallclock::WallClock;
use sp_core::genlock::floor_boundary_100ns;

/// How many boundaries ahead the pacer commits (#289, K). A cut lands
/// `CUT_LEAD_SLOTS` after the next boundary, so a boundary decided K ahead
/// is never decided before a cut that lands on it was clicked.
pub const AUDIO_LEAD_SLOTS: i64 = 1;

const _: () = assert!(CUT_LEAD_SLOTS >= AUDIO_LEAD_SLOTS);

/// The most waits [`Pacer::flush_line`] makes: a line K deep needs K
/// (1 in production), and a wait that returns early (`sleep_to_boundary`
/// bails on a backward step, its coarse sleep is clamped to 1 s) gets a few
/// more before what is left leaves at once.
const FLUSH_MAX_WAITS: usize = 4;

/// Which [`PacedSink`] method a committed pair leaves through.
enum Call {
    Emit(PacedFrame),
    Standby(PacedFrame),
    Blended(PacedFrame, FrameBlend),
    Shared {
        width: u32,
        height: u32,
        stride: u32,
        video: Picture,
    },
}

/// One committed pair: the call and its block and stamps.
struct Committed {
    call: Call,
    audio: Vec<AudioFrame>,
    video_tc_100ns: i64,
    audio_tc_100ns: i64,
}

impl Committed {
    /// Hand the pair to `sink` through the method that committed it.
    fn replay<S: PacedSink>(self, sink: &mut S) {
        let (audio, v, a) = (&self.audio[..], self.video_tc_100ns, self.audio_tc_100ns);
        match self.call {
            Call::Emit(frame) => sink.emit(&frame, audio, v, a),
            Call::Standby(frame) => sink.emit_standby(&frame, audio, v, a),
            Call::Blended(frame, blend) => sink.emit_blended(&frame, blend, audio, v, a),
            Call::Shared {
                width,
                height,
                stride,
                video,
            } => sink.submit_shared(width, height, stride, video, audio, v, a),
        }
    }
}

/// The pairs committed and not yet handed out, oldest first.
#[derive(Default)]
pub(super) struct CommitLine {
    pairs: VecDeque<Committed>,
    /// #289 step 2: how long (100 ns) before its stamp a pair leaves.
    hand_ahead_100ns: i64,
}

impl CommitLine {
    fn push(&mut self, call: Call, audio: &[AudioFrame], video_tc_100ns: i64, audio_tc_100ns: i64) {
        self.pairs.push_back(Committed {
            call,
            audio: audio.to_vec(),
            video_tc_100ns,
            audio_tc_100ns,
        });
    }

    /// Hand out, oldest first, every pair whose stamp the real wall `now`
    /// reached (or comes within the hand-ahead of), and a pair stranded more
    /// than `stranded_after` ahead of it (a backward wall step).
    fn release<S: PacedSink>(&mut self, now: i64, stranded_after: i64, sink: &mut S) {
        while let Some(front) = self.pairs.front() {
            let ahead = front.video_tc_100ns - now;
            if ahead > self.hand_ahead_100ns && ahead <= stranded_after {
                return;
            }
            let pair = self.pairs.pop_front().expect("the front was just read");
            pair.replay(sink);
        }
    }

    /// Hand out every pair at once.
    fn release_all<S: PacedSink>(&mut self, sink: &mut S) {
        while let Some(pair) = self.pairs.pop_front() {
            pair.replay(sink);
        }
    }

    fn front_stamp(&self) -> Option<i64> {
        self.pairs.front().map(|p| p.video_tc_100ns)
    }
}

impl PacedSink for CommitLine {
    fn emit(&mut self, video: &PacedFrame, audio: &[AudioFrame], vtc: i64, atc: i64) {
        self.push(Call::Emit(video.clone()), audio, vtc, atc);
    }

    fn emit_standby(&mut self, video: &PacedFrame, audio: &[AudioFrame], vtc: i64, atc: i64) {
        self.push(Call::Standby(video.clone()), audio, vtc, atc);
    }

    fn emit_blended(
        &mut self,
        video: &PacedFrame,
        blend: FrameBlend,
        audio: &[AudioFrame],
        vtc: i64,
        atc: i64,
    ) {
        self.push(Call::Blended(video.clone(), blend), audio, vtc, atc);
    }

    fn submit_shared(
        &mut self,
        width: u32,
        height: u32,
        stride: u32,
        video: Picture,
        audio: &[AudioFrame],
        vtc: i64,
        atc: i64,
    ) {
        let call = Call::Shared {
            width,
            height,
            stride,
            video,
        };
        self.push(call, audio, vtc, atc);
    }
}

impl Pacer {
    /// A pacer over `wall` that commits `lead_slots` boundaries ahead (#289;
    /// production: [`AUDIO_LEAD_SLOTS`], via [`Pacer::new`]).
    pub fn with_lead(grid_fps: i64, enabled: bool, wall: WallClock, lead_slots: i64) -> Self {
        let mut pacer = Self::with_wallclock(grid_fps, enabled, wall);
        pacer.lead_100ns = lead_slots * pacer.interval_100ns;
        pacer
    }

    /// #289 step 2: hand each committed pair out at once, the lead before
    /// its stamp, instead of at its stamp (production, via [`Pacer::new`]).
    pub fn handing_ahead(mut self) -> Self {
        self.line.hand_ahead_100ns = self.lead_100ns;
        self
    }

    /// How far (100 ns) before its stamp a committed pair leaves.
    pub fn hand_ahead_100ns(&self) -> i64 {
        self.line.hand_ahead_100ns
    }

    /// How far (100 ns) the decisions run ahead of the wall.
    pub fn lead_100ns(&self) -> i64 {
        self.lead_100ns
    }

    /// The REAL timeline now: the wall a committed pair waits for.
    pub fn wall_now_100ns(&self) -> i64 {
        self.wall.now_100ns()
    }

    /// Service one scheduling step (`service_now`, the decision, with the
    /// clock [`now_100ns`](Pacer::now_100ns)), through the commit line:
    /// each pair leaves into `sink` at its stamp.
    pub fn service<F, S>(&mut self, pull: F, sink: &mut S) -> ServiceOutcome
    where
        F: FnMut() -> Option<PacedFrame>,
        S: PacedSink,
    {
        self.through_line(sink, |pacer, line| pacer.service_now(pull, line))
    }

    /// Service one STANDBY step (`service_standby_now`) through the commit
    /// line, like [`service`](Pacer::service).
    pub fn service_standby<S: PacedSink>(
        &mut self,
        standby: Standby,
        sink: &mut S,
    ) -> ServiceOutcome {
        self.through_line(sink, |pacer, line| pacer.service_standby_now(standby, line))
    }

    /// Hand out every committed pair at its stamp: `wait(pacer, until)` sleeps
    /// until the decision clock reads `until` (production:
    /// `pipeline_paced::sleep_to_boundary`), so a pair stamped B leaves when
    /// the wall reaches B. At most `FLUSH_MAX_WAITS` waits; what is left then leaves
    /// at once. A scope end calls it before it hands the grid back.
    pub fn flush_line<S, W>(&mut self, sink: &mut S, mut wait: W)
    where
        S: PacedSink,
        W: FnMut(&Pacer, i64),
    {
        for _ in 0..FLUSH_MAX_WAITS {
            match self.release_due(sink) {
                Some(stamp) => wait(&*self, stamp + self.lead_100ns),
                None => return,
            }
        }
        self.line.release_all(sink);
    }

    /// Wait, handing out the pairs due meanwhile, until the REAL wall stands
    /// one slot before the next boundary: where a pacer with no lead stands
    /// after a pre-roll. The paced pipeline sends a song's `Started` there,
    /// so it still reaches the engine (the title clock) one slot before the
    /// song's first pair leaves; the decision clock then reads that boundary,
    /// so the first commit is not late. With no lead the wall is already
    /// past it (`wait` returns at once).
    pub fn wait_one_slot_before_next<S, W>(&mut self, sink: &mut S, mut wait: W)
    where
        S: PacedSink,
        W: FnMut(&Pacer, i64),
    {
        let previous = floor_boundary_100ns(self.next_boundary_100ns - 1, self.grid_fps);
        let until = previous + self.lead_100ns;
        wait(&*self, until);
        self.release_due(sink);
    }

    /// The song position (ms) to report for `decoded_ms`, the frame just
    /// taken for the boundary being committed: the lead earlier, so a
    /// `Position` reads the frame on the output now, as before #289 (the
    /// lyrics).
    pub fn shown_position_ms(&self, decoded_ms: u64) -> u64 {
        let lead_ms = (self.lead_100ns / 10_000) as u64;
        decoded_ms.saturating_sub(lead_ms)
    }

    /// Release the due pairs, run `step` with the line as its sink, release
    /// again (with no lead the step's own pairs leave here).
    fn through_line<S, T>(
        &mut self,
        sink: &mut S,
        step: impl FnOnce(&mut Pacer, &mut CommitLine) -> T,
    ) -> T
    where
        S: PacedSink,
    {
        // B leaves before B + K's decision work (a pre-decode, a blend).
        self.release_due(sink);
        let mut line = std::mem::take(&mut self.line);
        let out = step(self, &mut line);
        self.line = line;
        self.release_due(sink);
        out
    }

    /// Hand out the pairs the real wall reached (or stranded by a backward
    /// step); returns the stamp of the next one still waiting.
    fn release_due<S: PacedSink>(&mut self, sink: &mut S) -> Option<i64> {
        let stranded_after = self.lead_100ns + self.interval_100ns;
        let now = self.wall_now_100ns();
        self.line.release(now, stranded_after, sink);
        self.line.front_stamp()
    }

    /// The pairs committed and not yet handed out.
    #[cfg(test)]
    pub(crate) fn committed(&self) -> usize {
        self.line.pairs.len()
    }
}
