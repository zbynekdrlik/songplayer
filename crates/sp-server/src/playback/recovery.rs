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
use crate::resolume::{RecoveryEvent, ResolumeCommand, ResolumeRegistry};

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

/// The Resolume registry with a host driver per `(id, host, port)`, its
/// `RecoveryEvent` forwarder subscribed BEFORE the first driver starts
/// (release 0.68.0 blocker 4, design record 5863318980).
/// `ResolumeRegistry::new` keeps no receiver, and a broadcast sent with none
/// is dropped: when `lib.rs` subscribed after the startup (up to ~55 s after
/// `add_host` started the drivers), a driver's startup not-ready → ready
/// re-sync was lost, and Arena's restored stale text stayed on the wall.
/// The forwarded events queue on `engine_tx` until the engine loop runs.
pub(crate) fn registry_with_forwarder(
    hosts: Vec<(i64, String, u16)>,
    engine_tx: mpsc::Sender<EngineCommand>,
    shutdown_tx: &broadcast::Sender<()>,
) -> ResolumeRegistry {
    let mut registry = ResolumeRegistry::new();
    let events = registry.subscribe_recovery();
    tokio::spawn(forward_recovery_events(
        events,
        engine_tx,
        shutdown_tx.subscribe(),
    ));
    for (host_id, host, port) in hosts {
        registry.add_host(host_id, host, port, shutdown_tx.subscribe());
    }
    registry
}

impl super::PlaylistPipeline {
    /// The title clock of the song whose title this pipeline could put on
    /// the wall (#217 addendum 3): `video_id`, played on program, with its own
    /// clock (fixed at its `Started`, the instants the title timers sleep
    /// until). Every Play clears the clock (`begin_play`), so the video check
    /// is defensive.
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

    /// Decide the wall's title (#217 addendum 3): the due title or none, and
    /// the instant it was decided at. `None` when the due song's title read
    /// failed: a transient error must not hide a title mid-song, so nothing
    /// is sent. Another candidate's failed read is logged and does not matter.
    ///
    /// The candidates' titles are read FIRST, one await per candidate. The
    /// due title is then decided at `Instant::now()`, and the callers send it
    /// with no await in between. A timer that fired during the reads is
    /// already past its instant, so the Resync agrees with it. Deciding before
    /// the reads let a hide timer's HideTitle land ahead of a Resync that
    /// still named the title, which superseded it (review round 2).
    pub(super) async fn decide_wall_title(&self) -> Option<(Option<String>, Instant)> {
        let candidates = self.title_candidates();
        let mut titles = Vec::with_capacity(candidates.len());
        for &(_, clock) in &candidates {
            let text = title::title_text(&self.pool, clock.video_id).await;
            titles.push((clock.video_id, text));
        }
        let now = Instant::now();
        let due = due_title_video(&candidates, now);
        debug!(?candidates, ?due, "title window");
        let mut title = None;
        for (video_id, text) in titles {
            match text {
                Ok(text) if due == Some(video_id) => title = text,
                Ok(_) => {}
                Err(e) if due == Some(video_id) => {
                    warn!(video_id, %e, "title resync: DB lookup failed — nothing sent");
                    return None;
                }
                Err(e) => warn!(video_id, %e, "title read failed — its title is not due"),
            }
        }
        Some((title, now))
    }

    /// Declare the wall's title to the Resolume driver (a `Resync`): the due
    /// title, or none. The driver acts only on a difference, so this is
    /// idempotent: it never re-runs a fade for a title that is up. Used by a
    /// Resolume recovery and after a Play (`resync_after_play`). The OBS
    /// scene-on calls `decide_wall_title` itself: it also re-arms the song's
    /// timers at the decision instant (`push_title_for_playing`).
    pub(super) async fn resync_wall_title(&self) -> Option<String> {
        let (title, _) = self.decide_wall_title().await?;
        title::send_resync(self.obs_cmd_tx.as_ref(), &self.resolume_tx, title.clone()).await;
        title
    }

    /// The wall's line of every playing, on-program pipeline, at its last
    /// reported position: `(playlist id, video id, ShowSubtitles)`. A
    /// pipeline with no line there (no lyrics, a blank plan position) has
    /// none. Used by a Resolume recovery and when a playlist goes off program
    /// (`wall_after_scene_off`).
    pub(super) fn on_program_lines(&self) -> Vec<(i64, i64, ResolumeCommand)> {
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
                let cmd = ResolumeCommand::ShowSubtitles {
                    en,
                    next_en,
                    sk,
                    next_sk,
                    suppress_en: pp.cached_suppress_en,
                };
                shows.push((playlist_id, video_id, cmd));
            }
        }
        shows
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
        let shows = self.on_program_lines();
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
            let _ = self.resolume_tx.send(ResolumeCommand::HideSubtitles).await;
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
