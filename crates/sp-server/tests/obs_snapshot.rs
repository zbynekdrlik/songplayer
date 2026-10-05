//! #219: the OBS client publishes its state (`ObsClient::snapshots`).
//!
//! The real `ObsClient` against a `FakeObsServer`: the client reads cg OBS's
//! scene transition at connect and again on cg OBS's transition events, asks
//! again until a read is answered, and publishes a disconnect and the
//! reconnect's program. (#221 L5 deleted the program follow, this file's
//! other consumer, with its end-to-end test; L6 deletes the scene detection
//! and the transition reader these tests pin.)
//!
//! Every wait is bounded (20 s); nothing sleeps for synchronisation.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use common::{FakeObsServer, FakeObsState};
use serde_json::{Value, json};
use sp_server::{db, obs};
use sqlx::SqlitePool;
use tokio::sync::{RwLock, broadcast, watch};

const TIMEOUT: Duration = Duration::from_secs(20);

/// cg OBS's `GetCurrentSceneTransition` data for a transition.
fn transition_data(name: &str, kind: &str, duration: Value) -> Value {
    json!({
        "transitionName": name,
        "transitionUuid": format!("uuid-{name}"),
        "transitionKind": kind,
        "transitionFixed": duration.is_null(),
        "transitionDuration": duration,
        "transitionConfigurable": true,
        "transitionSettings": {},
    })
}

fn transition(name: &str, kind: &str, duration_ms: Option<u32>) -> obs::ObsTransition {
    obs::ObsTransition {
        name: name.to_string(),
        kind: kind.to_string(),
        duration_ms,
    }
}

/// A fake cg OBS whose program shows `sp-fast` (playlist 7); `sp-slow` shows
/// playlist 8; its transition is a 300 ms Fade.
fn cg_obs() -> FakeObsState {
    let mut s = FakeObsState::default();
    for (input, stream, scene) in [
        ("sp-fast_video", "SP-fast", "sp-fast"),
        ("sp-slow_video", "SP-slow", "sp-slow"),
    ] {
        s.inputs.insert(input.into(), "ndi_source".into());
        s.input_settings.insert(
            input.into(),
            json!({ "ndi_source_name": format!("RESOLUME-SNV ({stream})") }),
        );
        s.scene_items.insert(
            scene.into(),
            vec![(input.into(), false, "ndi_source".into())],
        );
    }
    s.scene_list = vec!["sp-fast".into(), "sp-slow".into()];
    s.program_scene = Some("sp-fast".into());
    s.scene_transition = Some(transition_data("Fade", "fade_transition", json!(300)));
    s
}

/// The real OBS client connected to a fake cg OBS.
struct Rig {
    _pool: SqlitePool,
    fake: FakeObsServer,
    snapshots: watch::Receiver<obs::ObsSnapshot>,
    _rebuild: broadcast::Sender<()>,
    shutdown: broadcast::Sender<()>,
    _client: obs::ObsClient,
}

impl Rig {
    async fn start(cg: FakeObsState) -> Self {
        let pool = db::create_memory_pool().await.unwrap();
        db::run_migrations(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) VALUES \
             (7, 'ytfast', 'https://yt/f', 'SP-fast', 1), \
             (8, 'ytslow', 'https://yt/s', 'SP-slow', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let fake = FakeObsServer::spawn_with_state(cg).await;
        let (event_tx, _) = broadcast::channel::<obs::ObsEvent>(64);
        let (rebuild, rebuild_rx) = broadcast::channel::<()>(4);
        let (shutdown, shutdown_rx) = broadcast::channel::<()>(1);
        let client = obs::ObsClient::spawn(
            obs::ObsConfig {
                url: fake.url(),
                password: None,
            },
            pool.clone(),
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(RwLock::new(obs::ObsState::default())),
            event_tx,
            rebuild_rx,
            shutdown_rx,
        );
        Self {
            _pool: pool,
            fake,
            snapshots: client.snapshots(),
            _rebuild: rebuild,
            shutdown,
            _client: client,
        }
    }

    /// Wait until a published snapshot satisfies `done`; that snapshot.
    async fn snapshot_becomes(
        &mut self,
        what: &str,
        done: impl Fn(&obs::ObsSnapshot) -> bool,
    ) -> obs::ObsSnapshot {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            {
                let snapshot = self.snapshots.borrow_and_update();
                if done(&snapshot) {
                    return snapshot.clone();
                }
            }
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let changed = tokio::time::timeout(left, self.snapshots.changed()).await;
            match changed {
                Ok(Ok(())) => {}
                Ok(Err(e)) => panic!("the OBS client's snapshots closed: {e}"),
                Err(_) => panic!(
                    "never published: {what}; last {:?}",
                    *self.snapshots.borrow()
                ),
            }
        }
    }

    /// The `GetCurrentSceneTransition` requests cg OBS received.
    async fn transition_reads(&self) -> usize {
        self.fake
            .state()
            .await
            .requests
            .iter()
            .filter(|r| r["requestType"] == "GetCurrentSceneTransition")
            .count()
    }

    async fn stop(self) {
        let _ = self.shutdown.send(());
        self.fake.shutdown().await;
    }
}

#[tokio::test]
async fn the_client_reads_cg_obs_transition_at_connect_and_on_its_events() {
    let mut rig = Rig::start(cg_obs()).await;
    let first = rig
        .snapshot_becomes("the transition read at connect", |s| s.transition.is_some())
        .await;
    assert_eq!(
        first.transition,
        Some(transition("Fade", "fade_transition", Some(300)))
    );
    assert!(first.connected);
    // cg OBS's operator picks a Cut: the event makes the client read again.
    rig.fake
        .update_state(|s| {
            s.scene_transition = Some(transition_data("Cut", "cut_transition", Value::Null));
        })
        .await;
    rig.fake
        .push_event(
            "CurrentSceneTransitionChanged",
            json!({ "transitionName": "Cut" }),
        )
        .await;
    rig.snapshot_becomes("the Cut", |s| {
        s.transition == Some(transition("Cut", "cut_transition", None))
    })
    .await;
    // A new duration: read again too.
    rig.fake
        .update_state(|s| {
            s.scene_transition = Some(transition_data("Fade", "fade_transition", json!(800)));
        })
        .await;
    rig.fake
        .push_event(
            "CurrentSceneTransitionDurationChanged",
            json!({ "transitionDuration": 800 }),
        )
        .await;
    rig.snapshot_becomes("the 800 ms Fade", |s| {
        s.transition == Some(transition("Fade", "fade_transition", Some(800)))
    })
    .await;
    // An ANSWERED read is never retried, so each new value can only come from
    // the read its event woke (the exact count is not asserted: under a slow
    // runner a read may time out and be retried, which is correct too).
    assert!(rig.transition_reads().await >= 3);
    rig.stop().await;
}

/// Review round 5: a real disconnect is PUBLISHED as the default
/// (disconnected, nothing known) snapshot — a consumer reads a disconnect
/// only from it — and the reconnect publishes cg OBS's program again.
#[tokio::test]
async fn a_disconnect_is_published_and_the_reconnect_publishes_the_program_again() {
    let mut rig = Rig::start(cg_obs()).await;
    let program = |s: &obs::ObsSnapshot| {
        s.connected
            && s.current_scene.as_deref() == Some("sp-fast")
            && s.active_playlist_ids.len() == 1
            && s.active_playlist_ids.contains(&7)
            && s.lookup_failed.is_none()
    };
    rig.snapshot_becomes("cg OBS's program, sp-fast {7}", program)
        .await;
    rig.fake.close_client().await;
    rig.snapshot_becomes("the disconnect", |s| *s == obs::ObsSnapshot::default())
        .await;
    rig.snapshot_becomes("the reconnect's program, sp-fast {7}", program)
        .await;
    rig.stop().await;
}

#[tokio::test]
async fn a_transition_read_with_no_answer_is_asked_again_until_answered() {
    let mut cg = cg_obs();
    cg.scene_transition = None; // answered without a transition kind
    let mut rig = Rig::start(cg).await;
    rig.snapshot_becomes("connected", |s| s.connected).await;
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while rig.transition_reads().await < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the unanswered transition read was never asked again"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(rig.snapshots.borrow().transition, None, "still unknown");
    rig.fake
        .update_state(|s| {
            s.scene_transition = Some(transition_data("Fade", "fade_transition", json!(700)));
        })
        .await;
    rig.snapshot_becomes("the retry's answer", |s| {
        s.transition == Some(transition("Fade", "fade_transition", Some(700)))
    })
    .await;
    rig.stop().await;
}
