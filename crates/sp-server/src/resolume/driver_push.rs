//! A title or subtitle push as its own driver step. Split from `driver.rs`
//! for the 1000-line cap; a child module of `driver`.

use std::time::Instant;

use tracing::warn;

use super::HostDriver;
use crate::resolume::{ResolumeCommand, handlers};

impl HostDriver {
    /// One push command (title or subtitles) as its own driver step.
    pub(super) async fn run_push(&mut self, cmd: &ResolumeCommand, _now: Instant) {
        self.push(cmd).await;
    }

    /// Run one push command's handler, logging a failure.
    async fn push(&mut self, cmd: &ResolumeCommand) {
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
            ResolumeCommand::RefreshMapping | ResolumeCommand::Shutdown => return,
        };
        if let Err(e) = result {
            warn!(host = %self.host, %e, "{what} failed");
        }
    }
}
