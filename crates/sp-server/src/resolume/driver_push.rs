//! A title or subtitle push as its own driver step, and what a 404 on it
//! means (#217 addendum 2). Split from `driver.rs` for the 1000-line cap; a
//! child module of `driver`, so it reads the driver's private state.

use std::sync::atomic::Ordering;
use std::time::Instant;

use tracing::{debug, info, warn};

use super::{FULL_REFRESH_RETRY, FULL_REFRESH_TTL, FullRefreshReason, HostDriver};
use crate::resolume::{ResolumeCommand, handlers};

impl HostDriver {
    /// Record a push answered `404 Not Found`: the param or clip id is gone.
    /// Arena gives every clip and text param a new id on each relaunch
    /// (`#sp-subs` 1790510617970 → 1790518489097 on the box), so the clip map
    /// is stale. Called at the one push choke point (`set_text` /
    /// `set_clip_opacity`); `push` reads and clears it.
    pub(super) fn note_push_status(&self, status: reqwest::StatusCode) {
        if status == reqwest::StatusCode::NOT_FOUND {
            self.stale_id_seen.store(true, Ordering::Relaxed);
        }
    }

    /// One push command (title or subtitles) as its own driver step.
    ///
    /// A push answered 404 marks the clip map stale. An Arena relaunch
    /// quicker than three failed probes never opens the breaker, so without
    /// this every push went to the dead ids until the 300 s TTL refresh. The
    /// stale map starts a not-ready episode, the refresh runs through the
    /// existing NotReady path (`decide`: a failed last attempt still waits
    /// out the retry window), and when it maps SongPlayer's clips anew the
    /// push is retried once.
    ///
    /// That refresh ends the episode with a RecoveryEvent, whose engine
    /// re-push sends the title and the current line after this step. So a
    /// ShowTitle is not retried here: a second ShowTitle restarts the title
    /// fade, the blink the one-event-per-step rule prevents.
    ///
    /// A refresh that maps the SAME clips is no relaunch: Arena still lists
    /// the ids it answers 404 for. No retry (it would 404 again), no event
    /// (`refresh_mapping` fires none for an unchanged map), and no new stale
    /// mark for `FULL_REFRESH_RETRY`: otherwise every push would cost a
    /// ~14 MB fetch.
    pub(super) async fn run_push(&mut self, cmd: &ResolumeCommand, now: Instant) {
        // A command is its own step: an event from an earlier tick does not
        // cover the ready transition this push's refresh may find.
        self.recovery_sent_this_step = false;
        if !self.push(cmd).await {
            return;
        }
        if self
            .refused_ids_at
            .is_some_and(|at| now.duration_since(at) < FULL_REFRESH_RETRY)
        {
            debug!(
                host = %self.host,
                "Resolume answered 404 again for an id its composition still lists — not refreshing"
            );
            return;
        }
        // Start (or continue) the not-ready episode: an open one keeps its
        // start, so its fast window never extends.
        self.not_ready_since.get_or_insert(now);
        warn!(
            host = %self.host,
            "Resolume answered 404 for a clip or parameter id — the clip map is stale (Arena re-ids its clips on relaunch), refreshing it"
        );
        let Some(reason) = FullRefreshReason::decide(
            now,
            self.last_full_refresh_ok_at,
            self.last_full_attempt_at,
            self.not_ready_since,
            self.last_full_attempt_failed,
            FULL_REFRESH_TTL,
            FULL_REFRESH_RETRY,
            false,
            false,
        ) else {
            info!(
                host = %self.host,
                "stale Resolume clip map: the retry window holds the refresh, the not-ready ticks fetch it"
            );
            return;
        };
        let before = self.clip_mapping.clone();
        self.run_full_refresh(reason, now).await;
        if self.not_ready_since.is_some() {
            // Still not ready (the composition is loading, or the fetch
            // failed): the episode's ticks refetch, and its end fires the
            // RecoveryEvent that re-pushes the wall.
            return;
        }
        if self.clip_mapping == before {
            self.refused_ids_at = Some(now);
            warn!(
                host = %self.host,
                "Resolume still lists the ids it answered 404 for — not a relaunch: no retry, no new refresh for 60 s"
            );
            return;
        }
        if self.recovery_sent_this_step && matches!(cmd, ResolumeCommand::ShowTitle { .. }) {
            info!(
                host = %self.host,
                "stale Resolume clip map refreshed — the RecoveryEvent re-pushes the title"
            );
            return;
        }
        info!(
            host = %self.host,
            "stale Resolume clip map refreshed — retrying the push once"
        );
        self.push(cmd).await;
    }

    /// Run one push command's handler, logging a failure. Returns whether
    /// Arena answered 404 to any of its requests, and clears that note, so
    /// it is false again outside a push.
    async fn push(&mut self, cmd: &ResolumeCommand) -> bool {
        let (what, result) = match cmd {
            ResolumeCommand::ShowTitle { song, artist } => {
                ("show_title", handlers::show_title(self, song, artist).await)
            }
            ResolumeCommand::HideTitle => ("hide_title", handlers::hide_title(self).await),
            ResolumeCommand::ShowSubtitles {
                en,
                next_en,
                sk,
                next_sk,
                suppress_en,
            } => (
                "subtitle set",
                handlers::set_subtitles(
                    self,
                    en,
                    next_en,
                    sk.as_deref(),
                    next_sk.as_deref(),
                    *suppress_en,
                )
                .await,
            ),
            ResolumeCommand::HideSubtitles => {
                ("subtitle clear", handlers::clear_subtitles(self).await)
            }
            ResolumeCommand::RefreshMapping | ResolumeCommand::Shutdown => return false,
        };
        if let Err(e) = result {
            warn!(host = %self.host, %e, "{what} failed");
        }
        self.stale_id_seen.swap(false, Ordering::Relaxed)
    }
}
