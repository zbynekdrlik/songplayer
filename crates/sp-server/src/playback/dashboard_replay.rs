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
//! The mode (#225 unit 2): a recorded playlist's is the one the engine last
//! told; any other's is its ROW's (`playlists.playback_mode`,
//! `db::models_playlists::row_mode`), the mode its pipeline starts in. The
//! row is the mode's one persisted truth: a change writes it first, then
//! tells the engine (`api/routes_mode.rs`), which tells the dashboards
//! (`playlist_mode.rs::apply_mode`, a pipeline-less playlist included), so
//! the record and the row agree.
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

    /// The on-connect replay (module doc). `playlists` = every playlist in
    /// the DB with its row's mode, in order. A recorded playlist the list
    /// does not name (the DB read failed) follows, by id; it has no row to
    /// read, so a state it never had is told in the default mode.
    pub fn replay(&self, playlists: &[(i64, PlaybackMode)]) -> Vec<ServerMsg> {
        let map = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        let mut unlisted: Vec<(i64, PlaybackMode)> = map
            .keys()
            .copied()
            .filter(|id| !playlists.iter().any(|(listed, _)| listed == id))
            .map(|id| (id, PlaybackMode::default()))
            .collect();
        unlisted.sort_unstable_by_key(|(id, _)| *id);

        let mut out = Vec::new();
        for (playlist_id, row_mode) in playlists.iter().copied().chain(unlisted) {
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
                    mode: recorded.map_or(row_mode, |(_, mode, _)| mode),
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
