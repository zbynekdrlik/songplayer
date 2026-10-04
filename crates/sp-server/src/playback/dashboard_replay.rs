//! #225: what a newly connected dashboard is told FIRST — the engine's last
//! dashboard message per playlist, replayed by the WS on-connect handler
//! (`api/websocket.rs::on_connect_replay`).
//!
//! Every `PlaybackStateChanged` and `NowPlaying` the engine sends goes through
//! [`PlaybackEngine::send_dashboard`], which records it here BEFORE it
//! broadcasts it. So a dashboard that connects mid-song is told exactly what
//! an already-connected one last got: the song (title, position, duration)
//! and the state, with the raw transport (#201). It is never the NDI-health
//! registry's 5 s sample, which kept a reload right after a cut on the old
//! program state until the playlist's next change.
//!
//! The replay ([`DashboardReplay::replay`]) covers EVERY playlist, so the
//! Player can tell "not known yet" (no state since this socket connected)
//! from "nothing plays":
//!
//! - a playlist whose last state is not `Idle`: its last `NowPlaying` (when it
//!   had one) FIRST, then its state, so a state never lands without its song;
//! - any other playlist: an explicit `Idle` state and no `NowPlaying`.
//!
//! The mode is the ENGINE's (review round 1): the recorded one, else
//! `PlaybackMode::default()`, the mode every pipeline starts with. The engine
//! never reads the DB's `playlists.playback_mode` and a mode change is not
//! written back, so the DB value is not what plays. A mode change broadcasts
//! the playlist's state (`handle_command`), so the record is never stale.
//!
//! A process-global, like `now_playing::global()` (#177): written by the
//! engine, read by the API without the engine's command channel, and no new
//! engine field (`playback/mod.rs` sits near the 1000-line cap). A test reads
//! its own instance, or only its own playlist ids.
//!
//! Nothing is lost between the replay and the live stream: the WS handler
//! subscribes to the event bus BEFORE it reads the replay, and the engine
//! records BEFORE it broadcasts, so a message is in the replay, or arrives
//! live, or both. A live message queued between the subscribe and the read
//! is delivered AFTER the replay even when it is older than the replayed
//! value, so a new client can briefly see the older value (a position a tick
//! back, the state before) until the next live message, a few ms later.

use std::collections::HashMap;
use std::sync::{OnceLock, PoisonError, RwLock};

use sp_core::playback::{PlaybackMode, PlaybackState, TransportState};
use sp_core::ws::ServerMsg;

use super::PlaybackEngine;

/// One playlist's last dashboard messages.
#[derive(Debug, Default)]
struct Entry {
    /// The last `PlaybackStateChanged`: its state, mode and transport.
    state: Option<(PlaybackState, PlaybackMode, TransportState)>,
    /// The last `NowPlaying`, as it was sent.
    now_playing: Option<ServerMsg>,
}

/// The engine's last dashboard message per playlist (module doc).
#[derive(Debug, Default)]
pub struct DashboardReplay {
    inner: RwLock<HashMap<i64, Entry>>,
}

impl DashboardReplay {
    /// Record a message the engine is about to send. Only
    /// `PlaybackStateChanged` and `NowPlaying` are kept: nothing else is a
    /// playlist's dashboard state.
    pub fn record(&self, msg: &ServerMsg) {
        let mut map = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        match msg {
            ServerMsg::PlaybackStateChanged {
                playlist_id,
                state,
                mode,
                transport,
            } => {
                map.entry(*playlist_id).or_default().state = Some((*state, *mode, *transport));
            }
            ServerMsg::NowPlaying { playlist_id, .. } => {
                map.entry(*playlist_id).or_default().now_playing = Some(msg.clone());
            }
            _ => {}
        }
    }

    /// Forget a playlist whose pipeline is gone (a runtime delete or
    /// deactivate): it then replays as `Idle`, or not at all once its row is
    /// gone from the DB.
    pub fn forget(&self, playlist_id: i64) {
        let mut map = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        map.remove(&playlist_id);
    }

    /// The on-connect replay (module doc). `playlist_ids` = every playlist in
    /// the DB, in order. A recorded playlist the list does not name (the DB
    /// read failed) follows, by id.
    pub fn replay(&self, playlist_ids: &[i64]) -> Vec<ServerMsg> {
        let map = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        let mut unlisted: Vec<i64> = map
            .keys()
            .copied()
            .filter(|id| !playlist_ids.contains(id))
            .collect();
        unlisted.sort_unstable();

        let mut out = Vec::new();
        for playlist_id in playlist_ids.iter().copied().chain(unlisted) {
            let entry = map.get(&playlist_id);
            let recorded = entry.and_then(|e| e.state);
            match recorded {
                Some((state, mode, transport)) if state != PlaybackState::Idle => {
                    out.extend(entry.and_then(|e| e.now_playing.clone()));
                    out.push(ServerMsg::PlaybackStateChanged {
                        playlist_id,
                        state,
                        mode,
                        transport,
                    });
                }
                _ => out.push(ServerMsg::PlaybackStateChanged {
                    playlist_id,
                    state: PlaybackState::Idle,
                    mode: recorded.map(|(_, mode, _)| mode).unwrap_or_default(),
                    transport: TransportState::Idle,
                }),
            }
        }
        out
    }
}

/// The process-global replay the engine records into and the WS handler
/// reads.
pub fn global() -> &'static DashboardReplay {
    static REPLAY: OnceLock<DashboardReplay> = OnceLock::new();
    REPLAY.get_or_init(DashboardReplay::default)
}

impl PlaybackEngine {
    /// The ONE way the engine tells the dashboard a playlist's state or song
    /// (`PlaybackStateChanged`, `NowPlaying`): record it for the on-connect
    /// replay, then broadcast it.
    pub(super) fn send_dashboard(&self, msg: ServerMsg) {
        global().record(&msg);
        let _ = self.ws_event_tx.send(msg);
    }
}

#[cfg(test)]
#[path = "dashboard_replay_tests.rs"]
mod tests;
