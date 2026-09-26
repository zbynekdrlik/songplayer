//! The paced song-start pre-roll (#147 song-start hole, ROZHODNUTÉ comment
//! 5842127369).
//!
//! When a song starts (idle→play, stop→play, song end→next song), the decoder
//! still has to open and decode its first frame. The pipeline used to block on
//! that with no boundary serviced, then `anchor()` the song's grid. The receiver
//! saw a hole of (open + first decode) at every song start. The sender contract
//! forbids a hole, and a hole at a song start is exactly what triggers the
//! receiver re-measure this ticket removes.
//!
//! [`Pacer::preroll`] keeps servicing every boundary with the standby pair (the
//! idle black + one silent block, the SAME path as any boundary) until the
//! caller's readiness `poll` reports the song can start. Only then does it
//! anchor, so the song's first frame is due on the very next boundary.
//! [`PrerollGate`] is the pure readiness decision the paced pipeline polls:
//! the decoder opened (or failed to) AND its first frame is buffered.
//!
//! The pre-roll's black also becomes the pacer's STANDBY FILL: any later
//! boundary with nothing of the song to show (a first frame whose pts lands
//! after the anchor, a pause before the first frame, an empty file) still
//! carries the black + silence pair instead of a hole (`Pacer::fill_starved`).
//! A same-song seek refill holds the last pre-seek picture instead
//! ([`Pacer::anchor_seek`]), so a seek never flashes black.
//!
//! A child of `pacer.rs` (1000-line cap), like `pacer_prepare.rs`.

use super::{PacedSink, Pacer, ServiceOutcome, Standby};
use crate::playback::frame_buf::SharedFrame;

/// A standby picture by reference: the idle / pre-roll black (or a held
/// frame), an NV12 frame of `width`×`height` (stride `stride`), submitted by
/// shared reference every boundary.
#[derive(Clone, Copy)]
pub struct StandbyBlack<'a> {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub video: &'a SharedFrame,
}

/// An owned standby picture (an `Arc` clone, no pixel copy): the pre-roll's
/// black, or the last pre-seek frame a seek refill holds (#147).
#[derive(Clone, Debug)]
pub(super) struct FillFrame {
    width: u32,
    height: u32,
    stride: u32,
    video: SharedFrame,
}

impl FillFrame {
    /// This picture as a borrowed [`StandbyBlack`] for the standby pair.
    fn view(&self) -> StandbyBlack<'_> {
        StandbyBlack {
            width: self.width,
            height: self.height,
            stride: self.stride,
            video: &self.video,
        }
    }
}

impl From<StandbyBlack<'_>> for FillFrame {
    fn from(black: StandbyBlack<'_>) -> Self {
        Self {
            width: black.width,
            height: black.height,
            stride: black.stride,
            video: black.video.clone(),
        }
    }
}

/// The pacer's fill for starved boundaries (#147): the pre-roll's black, and
/// after a same-song seek the last pre-seek picture (`hold`), which the refill
/// shows instead of flashing black. A new song's pre-roll drops the hold.
#[derive(Clone, Debug)]
pub(super) struct StandbyFill {
    black: FillFrame,
    hold: Option<FillFrame>,
}

impl<'a> StandbyBlack<'a> {
    /// This black as an idle [`Standby::Black`] boundary.
    pub fn standby(self) -> Standby<'a> {
        Standby::Black {
            width: self.width,
            height: self.height,
            stride: self.stride,
            video: self.video,
        }
    }
}

impl Pacer {
    /// Fill every grid boundary with the standby pair (`black` + one silent
    /// block, via [`service_standby`](Pacer::service_standby) into `sink`)
    /// until `poll` returns `Some`, then [`anchor`](Pacer::anchor) the song's
    /// grid and return that value (#147 song-start hole).
    ///
    /// `poll` is asked once per slot, right before the wait to the next
    /// boundary. A song that is ready anchors ON the boundary that wait was for
    /// (never on a re-read clock, so a preemption between the poll and the
    /// anchor cannot skip it): a pts-0 first frame lands there, and no boundary
    /// is skipped or doubled. `black` also becomes the pacer's standby fill
    /// (`fill_starved`). `wait(pacer, until)` sleeps to a
    /// boundary (production: `pipeline_paced::sleep_to_boundary`; tests: set
    /// the fake clock).
    pub fn preroll<S, P, W, T>(
        &mut self,
        black: StandbyBlack<'_>,
        sink: &mut S,
        mut poll: P,
        mut wait: W,
    ) -> T
    where
        S: PacedSink,
        P: FnMut() -> Option<T>,
        W: FnMut(&Pacer, i64),
    {
        self.standby_fill = Some(StandbyFill {
            black: FillFrame::from(black),
            hold: None,
        });
        loop {
            let step = self.service_standby(black.standby(), &mut *sink);
            let ServiceOutcome::Wait { until_100ns } = step else {
                // A boundary was just filled; the next step waits for the next.
                self.tick_wall();
                continue;
            };
            if let Some(ready) = poll() {
                self.anchor_at(until_100ns);
                return ready;
            }
            wait(&*self, until_100ns);
        }
    }

    /// Continue the grid right after the paced output's last serviced stamp
    /// `last_serviced_100ns` (#147, design record 5845527884, Approach 1 (a)):
    /// the next boundary this pacer latches is exactly one slot after it, so
    /// the stamps stay contiguous across a song change, a stop or an idle
    /// stretch while the pipeline-lifetime submit consumer filled the gap. A
    /// boundary the clock already passed is a normal catch-up (> 8 slots
    /// behind resyncs, the WARN path).
    ///
    /// RED stub: not wired yet — the pacer keeps its own `next_boundary`.
    pub fn continue_grid_after(&mut self, _last_serviced_100ns: i64) {}

    /// Re-anchor for a same-song SEEK (#147): like [`anchor`](Pacer::anchor),
    /// but the refill boundaries (nothing decoded at the new position yet) hold
    /// the last pre-seek picture with the standby silence, instead of flashing
    /// the pre-roll's black. A repeated seek before any new frame keeps the
    /// earlier hold. Without a fill (no pre-roll ran) it is a plain `anchor`.
    pub fn anchor_seek(&mut self) {
        let held = self.last_frame.take().map(|f| FillFrame {
            width: f.width,
            height: f.height,
            stride: f.stride,
            video: f.video,
        });
        self.anchor();
        if let Some(fill) = self.standby_fill.as_mut() {
            fill.hold = held.or(fill.hold.take());
        }
    }

    /// A boundary with nothing of the song to show (#147): with a standby fill
    /// set (by [`preroll`](Pacer::preroll)), it still carries the standby pair
    /// (the held pre-seek picture, else the black), so the output never has a
    /// hole. Without a fill (the SDK-clocked path never sets one, and neither do
    /// the unit tests that pin a bare starve) nothing is sent. Returns
    /// [`ServiceOutcome::Starved`] either way.
    pub(super) fn fill_starved<S: PacedSink>(
        &mut self,
        emit_now: i64,
        stamp_boundary: i64,
        audio_tc: i64,
        sink: &mut S,
    ) -> ServiceOutcome {
        if let Some(fill) = self.standby_fill.clone() {
            let picture = fill.hold.as_ref().unwrap_or(&fill.black);
            self.emit_standby_pair(emit_now, stamp_boundary, audio_tc, picture.view(), sink);
        }
        ServiceOutcome::Starved
    }

    /// The standby pair (#147), the ONE path an idle black, a pre-roll black
    /// and a starve fill leave through: one audio block (`standby_block`:
    /// silence, or a held EOS tail) then the picture by shared reference (a
    /// refcount bump, no pixel copy, #203), stamped like any emit.
    pub(super) fn emit_standby_pair<S: PacedSink>(
        &mut self,
        emit_now: i64,
        stamp_boundary: i64,
        audio_tc: i64,
        picture: StandbyBlack<'_>,
        sink: &mut S,
    ) {
        self.on_emit(emit_now, stamp_boundary);
        let block = self.standby_block();
        sink.submit_shared(
            picture.width,
            picture.height,
            picture.stride,
            picture.video.clone(),
            &block,
            stamp_boundary,
            audio_tc,
        );
    }
}

/// The paced pipeline's pre-roll readiness (#147): the song may anchor once
/// the decoder has OPENED and its first frame is buffered (`primed`). A failed
/// open ends the pre-roll at once, so the caller reports the error. The open
/// result is kept across polls, so the open channel is read exactly once.
#[derive(Debug)]
pub struct PrerollGate<T, E> {
    opened: Option<Result<T, E>>,
}

impl<T, E> PrerollGate<T, E> {
    /// A gate whose decoder has not reported its open result yet.
    pub fn pending() -> Self {
        Self { opened: None }
    }

    /// One readiness poll. `open` reads the decoder's open result without
    /// blocking (`None` = still opening); it is called only until it returns
    /// `Some`. `primed` reports the first frame (or end-of-stream) buffered; it
    /// is consulted only after a successful open. Returns `Some` exactly when
    /// the pre-roll may end: `Some(Err)` on a failed open, `Some(Ok)` once
    /// opened AND primed.
    pub fn poll<O, R>(&mut self, open: O, primed: R) -> Option<Result<T, E>>
    where
        O: FnOnce() -> Option<Result<T, E>>,
        R: FnOnce() -> bool,
    {
        if self.opened.is_none() {
            self.opened = open();
        }
        let opened_and_primed = matches!(self.opened, Some(Ok(_))) && primed();
        if opened_and_primed || matches!(self.opened, Some(Err(_))) {
            self.opened.take()
        } else {
            None
        }
    }
}
