//! Extracted from mod.rs to keep the file under the 1000-line cap.
//! Re-sync the wall after a Resolume host recovers (the title as a `Resync`,
//! the subtitle state as it is), and the forwarder that brings the driver's
//! `RecoveryEvent`s to the engine.

use std::sync::atomic::Ordering;

use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use super::state::PlayState;
use super::title::{self, TitleClock};
use crate::EngineCommand;
use crate::resolume::RecoveryEvent;

/// The `host` of the one `ResolumeRecovered` a lagged forwarder sends for the
/// events it missed. The engine re-pushes every host whatever the host.
pub(crate) const LAGGED_HOST: &str = "(lagged)";

/// Forward every Resolume `RecoveryEvent` to the engine as
/// `EngineCommand::ResolumeRecovered`, until shutdown or until the channel
/// closes (#217 addendum 3). It matches the whole `recv()` result: the old
/// `Ok(event) = recv()` select branch was disabled by the first error, so a
/// single `Lagged` ended recovery forwarding until shutdown. A lag means
/// events were missed, so a recovery may be pending: it forwards ONE event
/// for all of them. The re-push is idempotent, since the driver acts only on
/// a difference.
pub(crate) async fn forward_recovery_events(
    mut events: broadcast::Receiver<RecoveryEvent>,
    engine_tx: mpsc::Sender<EngineCommand>,
    mut shutdown: broadcast::Receiver<()>,
) {
    loop {
        let host = tokio::select! {
            received = events.recv() => match received {
                Ok(event) => event.host,
                Err(RecvError::Lagged(missed)) => {
                    warn!(missed, "Resolume recovery events lagged — forwarding one re-push for them");
                    LAGGED_HOST.to_string()
                }
                Err(RecvError::Closed) => return,
            },
            _ = shutdown.recv() => return,
        };
        let _ = engine_tx
            .send(EngineCommand::ResolumeRecovered { host })
            .await;
    }
}

impl super::PlaylistPipeline {
    /// The title clock of the song whose title this pipeline could put on
    /// the wall (#217 addendum 3): `video_id`, played on program, with its own
    /// clock (fixed at its `Started`, the instants the title timers sleep
    /// until). A clock of another video (the previous song, before the next
    /// `Started`) is none.
    fn on_air_clock(&self, video_id: i64) -> Option<TitleClock> {
        if !self.scene_active.load(Ordering::Acquire) {
            return None;
        }
        self.title_clock.filter(|clock| clock.video_id == video_id)
    }
}

/// The video whose title is due at `now` among `candidates` (`(playlist id,
/// clock)`): an open clock. Several (a program scene with more than one
/// SongPlayer playlist; they share the one `#sp-title` clip): the highest
/// playlist id, so the HashMap order never decides.
fn due_title_video(candidates: &[(i64, TitleClock)], now: Instant) -> Option<i64> {
    candidates
        .iter()
        .filter(|(_, clock)| clock.open_at(now))
        .max_by_key(|(playlist_id, _)| *playlist_id)
        .map(|(_, clock)| clock.video_id)
}

impl super::PlaybackEngine {
    /// The songs whose title could be on the wall: `(playlist id, clock)` of
    /// every playing, on-program pipeline with its own song's clock.
    fn title_candidates(&self) -> Vec<(i64, TitleClock)> {
        self.pipelines
            .iter()
            .filter_map(|(&playlist_id, pp)| match pp.state {
                PlayState::Playing { video_id } => {
                    pp.on_air_clock(video_id).map(|clock| (playlist_id, clock))
                }
                _ => None,
            })
            .collect()
    }

    /// Declare the wall's title to the Resolume driver (a `Resync`): the due
    /// title, or none. The driver acts only on a difference, so this is
    /// idempotent: it never re-runs a fade for a title that is up. Used by a
    /// Resolume recovery and by an OBS scene-on (#217 addendum 3).
    ///
    /// The candidates' titles are read FIRST, the one await. The due title is
    /// then decided at `Instant::now()` and sent at once. A hide timer that
    /// fired during the read is already past its instant, so this Resync
    /// agrees with it. Deciding before the read let such a timer's HideTitle
    /// land ahead of a Resync that still named the title, which superseded it
    /// (review round 2). A failed read sends nothing: a transient error must
    /// not hide a title mid-song.
    pub(super) async fn resync_wall_title(&self) -> Option<String> {
        let candidates = self.title_candidates();
        let mut titles = Vec::with_capacity(candidates.len());
        for &(_, clock) in &candidates {
            match title::title_text(&self.pool, clock.video_id).await {
                Ok(text) => titles.push((clock.video_id, text)),
                Err(e) => {
                    warn!(video_id = clock.video_id, %e, "title resync: DB lookup failed — nothing sent");
                    return None;
                }
            }
        }
        let due = due_title_video(&candidates, Instant::now());
        debug!(?candidates, ?due, "title window");
        let title = due
            .and_then(|video_id| titles.into_iter().find(|(id, _)| *id == video_id))
            .and_then(|(_, text)| text);
        title::send_resync(self.obs_cmd_tx.as_ref(), &self.resolume_tx, title.clone()).await;
        title
    }

    /// Re-sync a recovered Resolume host: the title as a `Resync` (the song
    /// title inside its window, else none) + the wall's subtitle state
    /// (ShowSubtitles for each on-program line, one HideSubtitles when there
    /// is none — also when no SongPlayer playlist is on program).
    pub(crate) async fn handle_resolume_recovery(&self, host: &str) {
        info!(
            host,
            "Resolume recovery — re-syncing the title and the subtitle state"
        );
        // The driver owns the title (#217 addendum 3): it compares this with
        // what it last did and cannot double-fade, flash, or show a title
        // outside its window, whatever is queued ahead of this Resync.
        let title = self.resync_wall_title().await;
        info!(host, ?title, "title re-synced on Resolume recovery");
        let mut shows = Vec::new();
        for (&playlist_id, pp) in &self.pipelines {
            let PlayState::Playing { video_id } = pp.state else {
                continue;
            };
            if !pp.scene_active.load(Ordering::Acquire) {
                continue;
            }
            let lines = pp.lyrics_state.as_ref().and_then(|state| {
                state.resolume_lines_with_next(pp.cached_position_ms, pp.cached_lyrics_reference)
            });
            if let Some((en, next_en, sk, next_sk)) = lines {
                let cmd = crate::resolume::ResolumeCommand::ShowSubtitles {
                    en,
                    next_en,
                    sk,
                    next_sk,
                    suppress_en: pp.cached_suppress_en,
                };
                shows.push((playlist_id, video_id, cmd));
            }
        }
        // Re-emit the wall's CURRENT subtitle state, a blank one included (a
        // blank plan position, a song without lyrics, or no SongPlayer
        // playlist on program). The engine's own hide for it
        // (`dispatch_lyrics_if_changed`, `clear_lyrics_display` at song
        // start, or the scene-off hide) was skipped against the host's empty
        // clip map and is not re-sent, so without this a stale text Arena
        // restored from its saved composition would stay. The subtitle clips
        // are shared by every on-program playlist, so the one Hide goes out
        // only when none of them has a line. The clear is instant (#217).
        if shows.is_empty() {
            let _ = self
                .resolume_tx
                .send(crate::resolume::ResolumeCommand::HideSubtitles)
                .await;
            info!(
                host,
                "subtitles cleared on Resolume recovery — no on-program line"
            );
        }
        for (playlist_id, video_id, cmd) in shows {
            let _ = self.resolume_tx.send(cmd).await;
            info!(
                playlist_id,
                video_id, "subtitle re-pushed on Resolume recovery"
            );
        }
    }
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod tests;
