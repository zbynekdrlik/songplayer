//! #229: the engine's half of the pause after failed opens (an `impl
//! PlaybackEngine` split out of `mod.rs` for the 1000-line cap).
//!
//! A failed Play (`PipelineEvent::Error`) used to select the next song at
//! once (`state.rs`: `(Playing, VideoError)` → `SelectAndPlay`), so a box
//! that cannot open any file skipped ~6 songs a second. Now each playlist
//! counts its failures in a row (`failure_backoff::FailureRun`). The 1st and
//! 2nd still select the next song at once; from the 3rd the playlist waits
//! (`WaitingForScene`, the state machine's own step) and ONE retry is armed
//! (`failure_backoff::next_attempt`: 5 s, 30 s, 120 s, then every 300 s).
//! The retry comes back on the engine's own event channel as
//! `PipelineEvent::RetryDue(id)`, as `scene_off.rs`'s re-check does, with an
//! id from a process-wide counter (never a tokio task id, which tokio may
//! reuse once its task has ended). Its attempt is a `PlayEvent::Start`: the
//! state machine selects the next song, and a ▶ does the same.
//!
//! A pending retry ends, and its `RetryDue` is then stale and ignored, on:
//!
//! - every Play (`begin_play`: a selection, a PlayVideo, Previous, a Loop
//!   replay);
//! - a `SceneOff` (a cut off program, or the dashboard's Pause);
//! - a skip, which tries the next song at once (`skip_backoff`);
//! - a song that starts (`Started`), which also resets the count.
//!
//! The run also keeps WHICH songs failed: a youtube selection leaves them
//! out, with the song just sent (`FailureRun::avoid`,
//! `failure_backoff::pick_pool`), so a song that never opened (and so is
//! never recorded as played) cannot be picked for good at the end of a
//! rotation.
//!
//! The health row shows the run (`open_failures`, `ndi_health.rs`), so a
//! black program has a visible reason on the dashboard. The engine writes
//! it as the run changes (`publish_open_failures`: a pause, a start, any
//! state event, a pick), not only at the pipeline's 5 s heartbeat.
//!
//! A `Started` or an `Error` names no Play. After a quick Play → Play, the
//! first song's answer can come after the second Play went out; it records
//! nothing, resets nothing and counts no failure (`answers_last_play`,
//! `failure_backoff::PlayAnswers`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use sp_core::playback::OpenFailures;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use super::PlaybackEngine;
use super::failure_backoff::{FailureRun, utc_ms_after};
use super::pipeline::PipelineEvent;
use super::state::{PlayAction, PlayEvent, PlayState};

/// The id of each retry (`RetryDue`), unique in the process (as
/// `scene_off.rs`'s `NEXT_RE_CHECK`).
static NEXT_RETRY: AtomicU64 = AtomicU64::new(1);

/// The armed retry of a playlist's failed opens.
#[derive(Debug)]
pub(super) struct PendingRetry {
    /// Its id: only a `RetryDue` that carries it acts.
    pub(super) id: u64,
    /// When it fires (the engine's clock); the health row reports it in UTC.
    pub(super) due: Instant,
    /// Its sleeping task, aborted when the retry ends early.
    task: tokio::task::AbortHandle,
}

/// A playlist's failed opens in a row, and its pending retry.
#[derive(Debug, Default)]
pub(super) struct FailureState {
    /// The run of failed opens since the last song started.
    pub(super) run: FailureRun,
    /// The armed retry; `Some` = the playlist waits before its next song.
    pub(super) retry: Option<PendingRetry>,
}

impl FailureState {
    /// The pending retry is over (a Play, a `SceneOff`, a skip, a start):
    /// its task is cancelled, and a `RetryDue` already queued is stale.
    pub(super) fn cancel_retry(&mut self) {
        if let Some(retry) = self.retry.take() {
            retry.task.abort();
        }
    }

    /// Whether `id` is the pending retry. It is then taken: its task has run.
    fn take_due(&mut self, id: u64) -> bool {
        let due = self.retry.as_ref().is_some_and(|retry| retry.id == id);
        if due {
            self.retry = None;
        }
        due
    }

    /// The health row's `open_failures`: the run, and when its pending retry
    /// is due in UTC ms (`None` while none is pending). `null` while no open
    /// failed since the last song started.
    pub(super) fn view(&self) -> Option<OpenFailures> {
        let now = Instant::now();
        let utc_now_ms = chrono::Utc::now().timestamp_millis();
        let retry_at_ms = self
            .retry
            .as_ref()
            .map(|retry| utc_ms_after(utc_now_ms, retry.due.saturating_duration_since(now)));
        self.run.view(retry_at_ms)
    }
}

impl PlaybackEngine {
    /// A Play of `playlist_id` failed (`PipelineEvent::Error` with `error`):
    /// one more failure in the run. Where the state machine would select the
    /// next song at once (`Playing` + `VideoError`), the 3rd and every later
    /// failure in a row wait instead ([`Self::back_off`]); otherwise the
    /// event goes through the state machine as before.
    pub(super) async fn video_failed(&mut self, playlist_id: i64, error: &str) {
        let event = PlayEvent::VideoError(error.to_owned());
        let backoff = {
            let Some(pp) = self.pipelines.get_mut(&playlist_id) else {
                warn!(playlist_id, "a failed open of a playlist with no pipeline");
                return;
            };
            let delay = pp.failures.run.fail(error);
            pp.failures.run.note_failed(pp.current_video_id);
            let (next, action) = pp.state.clone().transition(event.clone(), pp.mode);
            delay
                .filter(|_| action == Some(PlayAction::SelectAndPlay))
                .map(|delay| (next, delay))
        };
        match backoff {
            Some((next, delay)) => self.back_off(playlist_id, next, error, delay),
            None => self.apply_event(playlist_id, event).await,
        }
    }

    /// The state machine's `VideoError` step without its selection: the
    /// playlist waits in `next` (`WaitingForScene`), and ONE retry is armed
    /// `delay` from now. One WARN per pause names the playlist, the count and
    /// the last error.
    fn back_off(&mut self, playlist_id: i64, next: PlayState, error: &str, delay: Duration) {
        let tx = self.event_tx.clone();
        let Some(pp) = self.pipelines.get_mut(&playlist_id) else {
            return;
        };
        let id = NEXT_RETRY.fetch_add(1, Ordering::Relaxed);
        let due = Instant::now() + delay;
        let task = tokio::spawn(async move {
            tokio::time::sleep_until(due).await;
            let _ = tx.send((playlist_id, PipelineEvent::RetryDue(id)));
        })
        .abort_handle();
        pp.failures.cancel_retry(); // one retry per playlist
        pp.failures.retry = Some(PendingRetry { id, due, task });
        pp.state = next;
        warn!(
            playlist_id,
            failures = pp.failures.run.count(),
            error,
            retry_in_s = delay.as_secs(),
            "videos cannot be opened — the next attempt waits"
        );
        self.broadcast_state(playlist_id);
        self.publish_open_failures(playlist_id);
    }

    /// The health row's `open_failures` now (`NdiHealthRegistry::set_open_failures`),
    /// as the run of failed opens changes; each heartbeat copies it too.
    pub(super) fn publish_open_failures(&self, playlist_id: i64) {
        let open_failures = self
            .pipelines
            .get(&playlist_id)
            .and_then(|pp| pp.failures.view());
        self.ndi_health_registry
            .set_open_failures(playlist_id, open_failures);
    }

    /// `PipelineEvent::RetryDue(id)`: the pause is over and the next song is
    /// tried (`PlayEvent::Start`, the state machine's selection), unless `id`
    /// is not the pending retry: a Play, a `SceneOff`, a skip or a start
    /// ended it, so this one is stale.
    pub(super) async fn retry_due(&mut self, playlist_id: i64, id: u64) {
        let due = self
            .pipelines
            .get_mut(&playlist_id)
            .is_some_and(|pp| pp.failures.take_due(id));
        if !due {
            debug!(playlist_id, id, "a stale retry of a failed open — ignored");
            return;
        }
        info!(
            playlist_id,
            "the pause after failed opens is over — trying the next song"
        );
        self.apply_event(playlist_id, PlayEvent::Start).await;
    }

    /// A skip of `playlist_id` (`handle_command`) while a retry is pending:
    /// the next song is tried at once, and the retry is cancelled (the state
    /// machine ignores a skip while the playlist waits). Returns whether one
    /// was pending; `false` = the skip goes on as usual.
    pub(super) async fn skip_backoff(&mut self, playlist_id: i64) -> bool {
        let pending = self
            .pipelines
            .get_mut(&playlist_id)
            .and_then(|pp| pp.failures.retry.take());
        let Some(retry) = pending else {
            return false;
        };
        retry.task.abort();
        info!(
            playlist_id,
            "skipped during the pause after failed opens — trying the next song now"
        );
        self.apply_event(playlist_id, PlayEvent::Start).await;
        true
    }

    /// A `Started` or an `Error` of `playlist_id` came: whether it answers the
    /// LAST Play sent (`failure_backoff::PlayAnswers`). `false` = the answer
    /// to an earlier Play (a newer one is under way), which the event's arm
    /// ignores. With no pipeline, the arm handles the event as before.
    pub(super) fn answers_last_play(&mut self, playlist_id: i64) -> bool {
        self.pipelines
            .get_mut(&playlist_id)
            .is_none_or(|pp| pp.pending_plays.answered())
    }

    /// `PipelineEvent::Started` of `playlist_id`: a song opened, so the run of
    /// failed opens is over (its retry too). The song a SelectAndPlay or a
    /// PlayVideo marked (`record_on_start`) is recorded as played now, so a
    /// song that never started never uses up the unplayed rotation (#134's
    /// rule for both paths).
    pub(super) async fn song_started(&mut self, playlist_id: i64) {
        let Some(pp) = self.pipelines.get_mut(&playlist_id) else {
            return;
        };
        pp.failures.cancel_retry();
        if let Some(ended) = pp.failures.run.reset() {
            info!(
                playlist_id,
                failures = ended.count,
                last_error = %ended.last_error,
                "a song started — the run of failed opens is over"
            );
        }
        let record = pp.record_on_start.take();
        self.publish_open_failures(playlist_id);
        let Some(video_id) = record else {
            return;
        };
        if let Err(e) = crate::db::models::record_play(&self.pool, playlist_id, video_id).await {
            warn!(playlist_id, video_id, %e, "failed to record play");
        }
    }
}

#[cfg(test)]
#[path = "failure_retry_tests.rs"]
mod tests;
