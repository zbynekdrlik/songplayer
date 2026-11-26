//! The bus's audio side (#289 step 2, `program_audio.rs`): decide once,
//! read twice. A child of `program_bus.rs` (1000-line cap), like
//! `program_bus_window.rs`.
//!
//! With the audio side on ([`ProgramBus::with_audio_ahead`], production),
//! every boundary the core commits also queues its audio, moved out of the
//! job ([`take_audio`]), for the `program-audio` thread; the sender's job
//! keeps the picture. The sources then commit `lead_slots` ahead
//! (`pacer_commit.rs`), so a cut is placed `lead_slots` fewer slots after
//! the newest stamp seen: a cut clicked in (B, B + 1) still lands on B + 2.
//! Off (`ProgramCore::new`, the tests), nothing changes.

use std::sync::Arc;
use std::time::Duration;

use sp_core::genlock::{interval_100ns, strict_next_boundary_100ns};

use super::{
    CUT_LEAD_SLOTS, PROGRAM_QUEUE_BOUND, ProgramBus, ProgramCore, ProgramJob, ProgramStatus,
};
use crate::playback::program_audio::{
    AudioAheadStatus, AudioJob, AudioLine, LineStatus, MarginWindow, take_audio,
};
use crate::playback::program_output_timing::BoundaryMarks;
use crate::playback::submit_handoff::{HandoffOutcome, SubmitQueue};

/// The core's audio side.
#[derive(Default)]
pub(super) struct AudioAhead {
    lead_slots: i64,
    /// The audio jobs for the `program-audio` thread; `None` = off (the
    /// audio stays in the job, the sender limits it inline).
    queue: Option<SubmitQueue<AudioJob>>,
    coalesced: u64,
    margin: MarginWindow,
    /// The lead of the source whose boundary was committed last.
    last_lead_slots: i64,
}

impl AudioAhead {
    /// How far (100 ns) the sources commit ahead.
    pub(super) fn lead_100ns(&self, fps: i64) -> i64 {
        self.lead_slots * interval_100ns(fps)
    }

    /// The slots source `pid` commits ahead: a playlist's paced pipeline
    /// the lead; the NDI input "OBS manuál" (-1, stamped on its capture)
    /// and Blank (-2, nobody offers it) none.
    pub(super) fn lead_of(&self, pid: i64) -> i64 {
        if pid >= 0 { self.lead_slots } else { 0 }
    }

    /// The lead of the boundary committed last: its source's for a forward
    /// or a mix, none for the program's own fill.
    pub(super) fn last_lead(&self) -> i64 {
        self.last_lead_slots
    }

    /// Where a cut lands: for each stamp seen, the boundary after it plus
    /// [`CUT_LEAD_SLOTS`] less its source's lead (a click in (B, B + 1)
    /// lands on B + 2, whichever sources are ahead), the latest of them;
    /// with nothing seen, the caller's clock read the lead ahead.
    pub(super) fn cut_boundary(
        &self,
        fps: i64,
        seen: impl Iterator<Item = (i64, i64)>,
        now_100ns: i64,
    ) -> i64 {
        let mut seen = seen.peekable();
        let fallback = (now_100ns + self.lead_100ns(fps), self.lead_slots);
        let fallback = seen.peek().is_none().then_some(fallback);
        seen.chain(fallback)
            .map(|(stamp, lead)| {
                let mut boundary = strict_next_boundary_100ns(stamp, fps);
                for _ in 0..CUT_LEAD_SLOTS - lead {
                    boundary = strict_next_boundary_100ns(boundary, fps);
                }
                boundary
            })
            .max()
            .expect("a stamp or the fallback")
    }

    /// A boundary is committed: queue its audio (moved out of `job`) and
    /// note its source's lead (`source`, the one it shows).
    pub(super) fn split(&mut self, job: &mut ProgramJob, source: Option<i64>) {
        let fill = matches!(job, ProgramJob::Standby { .. });
        self.last_lead_slots = match source {
            Some(pid) if !fill => self.lead_of(pid),
            _ => 0,
        };
        if let Some(queue) = &mut self.queue
            && let HandoffOutcome::Coalesced { .. } = queue.offer(take_audio(job))
        {
            self.coalesced += 1;
        }
    }

    /// The sender served a boundary: its margin.
    pub(super) fn observe(&mut self, marks: &BoundaryMarks) {
        self.margin.observe(marks.stamp_100ns, marks.fed_100ns);
    }

    pub(super) fn status(&self) -> AudioAheadStatus {
        AudioAheadStatus {
            lead_slots: self.lead_slots,
            boundaries_ahead: self.margin.ahead(),
            margin_us_min: self.margin.margin_us_min(),
            coalesced: self.coalesced,
            ..AudioAheadStatus::default()
        }
    }
}

impl ProgramCore {
    /// The audio side on, the sources committing `lead_slots` ahead
    /// (production: `AUDIO_LEAD_SLOTS`; 0 moves only the audio out).
    pub fn with_audio_ahead(mut self, lead_slots: i64) -> Self {
        self.ahead.lead_slots = lead_slots;
        self.ahead.queue = Some(SubmitQueue::new(PROGRAM_QUEUE_BOUND));
        self
    }

    /// The next queued audio job, in commit order.
    pub fn take_audio(&mut self) -> Option<AudioJob> {
        self.ahead.queue.as_mut()?.take()
    }

    /// Audio jobs queued.
    pub fn audio_queued(&self) -> usize {
        self.ahead.queue.as_ref().map_or(0, SubmitQueue::depth)
    }
}

impl ProgramStatus {
    /// The status with the line's counters (the bus holds the line).
    pub(super) fn with_line(mut self, line: LineStatus) -> Self {
        self.health.audio.line = line;
        self
    }
}

/// What the `program-audio` thread got from [`ProgramBus::take_audio_timeout`].
pub enum AudioTake {
    Job(AudioJob),
    /// Nothing queued within the wait.
    Idle,
    /// The bus was stopped and the audio queue is drained.
    Stopped,
}

impl ProgramBus {
    /// See [`ProgramCore::with_audio_ahead`].
    pub fn with_audio_ahead(mut self, lead_slots: i64) -> Self {
        let st = self.state.get_mut().unwrap_or_else(|p| p.into_inner());
        st.core = std::mem::take(&mut st.core).with_audio_ahead(lead_slots);
        self
    }

    /// The line of limited blocks between the audio thread and the sender.
    pub fn audio_line(&self) -> &Arc<AudioLine> {
        &self.line
    }

    /// Wake both readers: something may have been committed.
    pub(super) fn notify(&self) {
        self.ready.notify_one();
        self.audio_ready.notify_one();
    }

    /// The `program-audio` thread: the next audio job, waiting at most `wait`.
    pub fn take_audio_timeout(&self, wait: Duration) -> AudioTake {
        let st = self.lock();
        let (mut st, _) = self
            .audio_ready
            .wait_timeout_while(st, wait, |s| s.core.audio_queued() == 0 && !s.stop)
            .unwrap_or_else(|p| p.into_inner());
        match st.core.take_audio() {
            Some(job) => AudioTake::Job(job),
            None if st.stop => AudioTake::Stopped,
            None => AudioTake::Idle,
        }
    }
}
