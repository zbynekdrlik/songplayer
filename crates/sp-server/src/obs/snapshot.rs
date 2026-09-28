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
//!
//! A scene apply writes through [`ObsShared::update_scene`] with a ticket
//! taken before the scene it applies was read ([`ObsShared::scene_ticket`]):
//! an answer that arrives after a LATER apply's answer was written is
//! dropped, so an out-of-order lookup, or a poll whose read an event already
//! overtook, never rolls the scene back.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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
    scene_order: Arc<SceneOrder>,
}

/// The order of the scene applies (#218 review): cg OBS may answer two
/// lookups out of order, and the poll's relookups can overlap an event's
/// lookup, so a late answer must never overwrite a newer one.
#[derive(Default)]
struct SceneOrder {
    /// The last ticket handed out.
    issued: AtomicU64,
    /// The ticket of the apply that wrote last.
    written: AtomicU64,
}

impl ObsShared {
    pub(crate) fn new(state: Arc<RwLock<ObsState>>) -> Self {
        let (tx, _) = watch::channel(ObsSnapshot::default());
        Self {
            state,
            tx,
            scene_order: Arc::default(),
        }
    }

    /// A ticket for one scene apply, taken BEFORE the information it applies
    /// was read: the connection loop takes an event's before spawning its
    /// apply (in event order); the poll and the initial read take theirs
    /// before asking cg OBS for its program scene. A later event then always
    /// outranks a read that answered before it. See [`Self::update_scene`].
    pub(crate) fn scene_ticket(&self) -> u64 {
        self.scene_order.issued.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// [`Self::update`] for the scene apply holding `ticket`: skipped (`None`)
    /// when an apply with a LATER ticket already wrote — its answer is newer.
    pub(crate) async fn update_scene<R>(
        &self,
        ticket: u64,
        f: impl FnOnce(&mut ObsState) -> R,
    ) -> Option<R> {
        let mut state = self.state.write().await;
        if ticket < self.scene_order.written.load(Ordering::SeqCst) {
            return None;
        }
        self.scene_order.written.store(ticket, Ordering::SeqCst);
        let out = f(&mut state);
        self.publish(&state);
        Some(out)
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
        self.publish(&state);
        out
    }

    /// Publish `state`'s snapshot when it differs from the last one (the
    /// caller holds the write lock).
    fn publish(&self, state: &ObsState) {
        let snapshot = ObsSnapshot::of(state);
        self.tx.send_if_modified(|current| {
            if *current == snapshot {
                return false;
            }
            *current = snapshot;
            true
        });
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
    async fn a_late_scene_answer_never_overwrites_a_newer_one() {
        let obs = ObsShared::new(Arc::new(RwLock::new(ObsState::default())));
        let older = obs.scene_ticket();
        let newer = obs.scene_ticket();
        assert!(newer > older, "tickets increase");
        // The newer apply's lookup answers first ...
        let wrote = obs
            .update_scene(newer, |s| s.current_scene = Some("sp-slow".to_string()))
            .await;
        assert_eq!(wrote, Some(()));
        // ... then the older one's: dropped, the newer scene stays.
        let late = obs
            .update_scene(older, |s| s.current_scene = Some("sp-fast".to_string()))
            .await;
        assert_eq!(late, None, "the late answer is dropped");
        assert_eq!(
            obs.subscribe().borrow().current_scene.as_deref(),
            Some("sp-slow")
        );
        // A later apply writes again, and a plain update is never ordered.
        let latest = obs.scene_ticket();
        assert_eq!(
            obs.update_scene(latest, |s| s.current_scene = Some("Slido".to_string()))
                .await,
            Some(())
        );
        obs.update(|s| s.connected = true).await;
        let snapshot = obs.subscribe().borrow().clone();
        assert_eq!(
            (snapshot.current_scene.as_deref(), snapshot.connected),
            (Some("Slido"), true)
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
