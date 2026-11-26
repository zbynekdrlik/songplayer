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

use sp_core::genlock::interval_100ns;

use super::{CUT_LEAD_SLOTS, PROGRAM_QUEUE_BOUND, ProgramBus, ProgramCore, ProgramJob};
use crate::playback::program_audio::{
    AudioAheadStatus, AudioJob, AudioLine, MarginWindow, take_audio,
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
}

impl AudioAhead {
    /// How far (100 ns) the sources commit ahead.
    pub(super) fn lead_100ns(&self, fps: i64) -> i64 {
        self.lead_slots * interval_100ns(fps)
    }

    /// The slots a cut is placed after the boundary following the newest
    /// stamp seen: [`CUT_LEAD_SLOTS`] less the sources' lead.
    pub(super) fn cut_lead_slots(&self) -> i64 {
        CUT_LEAD_SLOTS - self.lead_slots
    }

    /// A boundary is committed: queue its audio (moved out of `job`).
    pub(super) fn split(&mut self, job: &mut ProgramJob) {
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
