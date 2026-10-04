//! A title or subtitle push as its own driver step, and what a 404 on it
//! means (#217 addendum 2); a title push goes through the driver's title
//! state (#217 addendum 3). Split from `driver.rs` for the 1000-line cap; a
//! child module of `driver`, so it reads the driver's private state.

use std::sync::atomic::Ordering;
use std::time::Instant;

use tracing::{debug, info, warn};

use super::{FULL_REFRESH_RETRY, FULL_REFRESH_TTL, FullRefreshReason, HostDriver};
use crate::resolume::title_state::{TitleAction, TitleIntent};
use crate::resolume::{ResolumeCommand, handlers};

impl HostDriver {
    /// Test seam (#217): empty the endpoint cache, as its 5-minute TTL running
    /// out does, AFTER a handler resolved the endpoint for its batches.
    #[cfg(test)]
    pub(crate) fn forget_endpoint(&mut self) {
        self.endpoint_cache = None;
    }

    /// Record a push answered `404 Not Found`: the param or clip id is gone.
    /// Arena gives every clip and text param a new id on each relaunch
    /// (`#sp-subs` 1790510617970 → 1790518489097 on the box), so the clip map
    /// is stale. Called at the one push choke point (`set_text` /
    /// `set_clip_opacity`); `finish_push` reads and clears it.
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
    /// push is retried once (`retry_push`).
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
            debug!(
                host = %self.host,
                "Resolume answered 404 (a stale clip map): the retry window holds the refresh, the not-ready ticks fetch it"
            );
            return;
        };
        warn!(
            host = %self.host,
            "Resolume answered 404 for a clip or parameter id — the clip map is stale (Arena re-ids its clips on relaunch), refreshing it"
        );
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
        self.retry_push(cmd).await;
    }

    /// The one retry after a refresh that mapped SongPlayer's clips anew. That
    /// refresh has just fired this step's RecoveryEvent, whose engine re-push
    /// resyncs the title after the step and sends the current line. So:
    ///
    /// - a title to show (a ShowTitle, a Resync naming one) is left to that
    ///   Resync, which shows it only inside the engine's title window;
    /// - anything else runs again. A hide hides at once: the new clip ids
    ///   made the title state `Unknown` (or the failed hide left it
    ///   `FadingOut`), so it never fades restored text from full opacity. A
    ///   subtitle push is an instant, harmless double.
    async fn retry_push(&mut self, cmd: &ResolumeCommand) {
        match cmd {
            ResolumeCommand::ShowTitle { .. } | ResolumeCommand::Resync { title: Some(_) } => {
                info!(
                    host = %self.host,
                    "stale Resolume clip map refreshed — the RecoveryEvent re-syncs the title"
                );
            }
            _ => {
                info!(
                    host = %self.host,
                    "stale Resolume clip map refreshed — retrying the push once"
                );
                self.push(cmd).await;
            }
        }
    }

    /// Run one push command's handler (`finish_push`: log a failure, return
    /// and clear whether Arena answered 404). A title command goes through
    /// the title state (`push_title`).
    async fn push(&mut self, cmd: &ResolumeCommand) -> bool {
        let (what, result) = match cmd {
            ResolumeCommand::ShowTitle { song, artist } => {
                let text = handlers::format_title_text(song, artist);
                return self.push_title(TitleIntent::Show(&text)).await;
            }
            ResolumeCommand::HideTitle => return self.push_title(TitleIntent::Hide).await,
            ResolumeCommand::Resync { title } => {
                return self.push_title(TitleIntent::Resync(title.as_deref())).await;
            }
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
        self.finish_push(what, result)
    }

    /// One title command, acted on only where it differs from what the wall
    /// shows (#217 addendum 3, `TitleState::plan`).
    async fn push_title(&mut self, intent: TitleIntent<'_>) -> bool {
        let action = self.title.plan(intent);
        debug!(
            host = %self.host,
            ?intent,
            state = ?self.title.state(),
            ?action,
            "Resolume title command"
        );
        match action {
            Some(action) => self.run_title_action(action).await,
            None => false,
        }
    }

    /// Run a title action on the `#sp-title` clips and record it in the title
    /// state: between states while it runs, the new state once every request
    /// answered. Without title clips nothing runs and the state is kept (the
    /// clips a later refresh maps decide it). Returns the 404 note.
    async fn run_title_action(&mut self, action: TitleAction<'_>) -> bool {
        let Some(clips) = handlers::clips_for_title(self) else {
            debug!(host = %self.host, ?action, "no #sp-title clips mapped — title state kept");
            return false;
        };
        self.title.begin(action, clips);
        let (what, result) = match action {
            TitleAction::FadeIn(text) => ("show_title", handlers::show_title(self, text).await),
            TitleAction::Replace(text) => {
                ("replace_title", handlers::replace_title(self, text).await)
            }
            TitleAction::FadeOut => ("hide_title", handlers::hide_title(self).await),
            TitleAction::HideNow => ("hide_title_now", handlers::hide_title_now(self).await),
        };
        self.title.finish(result.is_ok());
        self.finish_push(what, result)
    }

    /// End one push: log a failure, and return whether Arena answered 404 to
    /// any of its requests, clearing that note, so it is false again outside
    /// a push.
    fn finish_push(&mut self, what: &str, result: Result<(), anyhow::Error>) -> bool {
        if let Err(e) = result {
            warn!(host = %self.host, %e, "{what} failed");
        }
        self.stale_id_seen.swap(false, Ordering::Relaxed)
    }
}

#[cfg(test)]
#[path = "driver_title_tests.rs"]
mod title_tests;
