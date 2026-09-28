//! #219: the OBS client's state, published to its consumers.
//!
//! `ObsState` (`Arc<RwLock<…>>`, read by the API and the idle gates) is the
//! OBS client's view of cg OBS. Its program part is ALSO published as an
//! [`ObsSnapshot`] on a `tokio::sync::watch` channel, so a consumer (the #215
//! program follow) reacts to each change instead of asking cg OBS again —
//! there is ONE view of cg OBS, the OBS client's. The contract:
//!
//! - `connected`: the OBS connection is up (identified);
//! - `current_scene` + `active_playlist_ids`: cg OBS's program scene and the
//!   SongPlayer playlists it shows (`scene::apply_scene_change`, also driven
//!   by the #170 poll);
//! - `lookup_failed`: `Some(current_scene)` while that scene's playlist
//!   lookup failed (#218). `active_playlist_ids` then still belongs to an
//!   EARLIER scene, so a consumer must not act on it; the scene poll looks
//!   the scene up again, and its success publishes the real set;
//! - `transition`: cg OBS's current scene transition, `None` while unknown
//!   (`transition.rs`).
//!
//! Every write of those fields goes through [`ObsShared::update`], which
//! publishes under the same write lock (a snapshot never shows a state the
//! lock did not hold, and snapshots are published in write order) and only
//! on a real change of the snapshot (a streaming / recording change wakes
//! nobody). A disconnect resets them all. The channel starts disconnected.

use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::{RwLock, watch};

use crate::obs::ObsState;
use crate::obs::transition::ObsTransition;

/// The published part of `ObsState` (see the module doc).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObsSnapshot {
    pub connected: bool,
    pub current_scene: Option<String>,
    pub active_playlist_ids: HashSet<i64>,
    pub lookup_failed: Option<String>,
    pub transition: Option<ObsTransition>,
}

impl ObsSnapshot {
    /// The snapshot of `state`.
    pub fn of(state: &ObsState) -> Self {
        Self {
            connected: state.connected,
            current_scene: state.current_scene.clone(),
            active_playlist_ids: state.active_playlist_ids.clone(),
            lookup_failed: state.lookup_failed.clone(),
            transition: state.transition.clone(),
        }
    }
}

/// The OBS client's shared state + the channel it is published on.
#[derive(Clone)]
pub(crate) struct ObsShared {
    state: Arc<RwLock<ObsState>>,
    tx: watch::Sender<ObsSnapshot>,
}

impl ObsShared {
    pub(crate) fn new(state: Arc<RwLock<ObsState>>) -> Self {
        let (tx, _) = watch::channel(ObsSnapshot::default());
        Self { state, tx }
    }

    /// The shared state, for reads and for the fields that are not published
    /// (streaming / recording).
    pub(crate) fn state(&self) -> &Arc<RwLock<ObsState>> {
        &self.state
    }

    /// A new receiver of the snapshots.
    pub(crate) fn subscribe(&self) -> watch::Receiver<ObsSnapshot> {
        self.tx.subscribe()
    }

    /// Change the state with `f` and publish the new snapshot (only when it
    /// differs from the last one), both under the one write lock.
    pub(crate) async fn update<R>(&self, f: impl FnOnce(&mut ObsState) -> R) -> R {
        let mut state = self.state.write().await;
        let out = f(&mut state);
        let snapshot = ObsSnapshot::of(&state);
        self.tx.send_if_modified(|current| {
            if *current == snapshot {
                return false;
            }
            *current = snapshot;
            true
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(ids: &[i64]) -> HashSet<i64> {
        ids.iter().copied().collect()
    }

    #[tokio::test]
    async fn an_update_publishes_the_program_part_of_the_state() {
        let state = Arc::new(RwLock::new(ObsState::default()));
        let obs = ObsShared::new(state.clone());
        let mut rx = obs.subscribe();
        assert_eq!(*rx.borrow_and_update(), ObsSnapshot::default());
        let fade = ObsTransition {
            name: "Fade".to_string(),
            kind: "fade_transition".to_string(),
            duration_ms: Some(500),
        };
        let returned = obs
            .update(|s| {
                s.connected = true;
                s.current_scene = Some("sp-fast".to_string());
                s.active_playlist_ids = set(&[7]);
                s.lookup_failed = Some("sp-fast".to_string());
                s.transition = Some(fade.clone());
                42
            })
            .await;
        assert_eq!(returned, 42, "update returns what the change returns");
        assert!(rx.has_changed().unwrap());
        assert_eq!(
            *rx.borrow_and_update(),
            ObsSnapshot {
                connected: true,
                current_scene: Some("sp-fast".to_string()),
                active_playlist_ids: set(&[7]),
                lookup_failed: Some("sp-fast".to_string()),
                transition: Some(fade),
            }
        );
        assert_eq!(
            state.read().await.current_scene.as_deref(),
            Some("sp-fast"),
            "the shared state holds the change"
        );
    }

    #[tokio::test]
    async fn a_change_outside_the_snapshot_wakes_nobody() {
        let obs = ObsShared::new(Arc::new(RwLock::new(ObsState::default())));
        let mut rx = obs.subscribe();
        let _ = rx.borrow_and_update();
        obs.update(|s| {
            s.streaming = true;
            s.recording = true;
        })
        .await;
        assert!(
            !rx.has_changed().unwrap(),
            "streaming / recording are not published"
        );
        obs.update(|s| s.connected = true).await;
        assert!(rx.has_changed().unwrap());
        let _ = rx.borrow_and_update();
        obs.update(|s| s.connected = true).await;
        assert!(
            !rx.has_changed().unwrap(),
            "the same snapshot again is no change"
        );
        assert!(obs.state().read().await.streaming);
    }
}
