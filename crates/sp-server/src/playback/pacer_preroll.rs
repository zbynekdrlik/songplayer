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
//! A child of `pacer.rs` (1000-line cap), like `pacer_prepare.rs`.

use super::{PacedSink, Pacer, ServiceOutcome, Standby};
use crate::playback::frame_buf::SharedFrame;

/// The idle black a pre-roll fills boundaries with: an NV12 frame of
/// `width`×`height` (stride `stride`), shared by reference every boundary.
#[derive(Clone, Copy)]
pub struct StandbyBlack<'a> {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub video: &'a SharedFrame,
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
    /// boundary, so a song that is ready anchors on the boundary that wait was
    /// for: the first frame lands there, and no boundary is skipped or doubled.
    /// `wait(pacer, until)` sleeps to a boundary (production:
    /// `pipeline_paced::sleep_to_boundary`; tests: set the fake clock).
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
        loop {
            match self.service_standby(black.standby(), &mut *sink) {
                ServiceOutcome::Wait { until_100ns } => {
                    if let Some(ready) = poll() {
                        self.anchor();
                        return ready;
                    }
                    wait(&*self, until_100ns);
                }
                _ => self.tick_wall(),
            }
        }
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
