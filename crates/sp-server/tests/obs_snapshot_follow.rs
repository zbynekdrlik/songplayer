//! #219: the OBS client publishes its state (`ObsClient::snapshots`) and the
//! program follow consumes only that.
//!
//! The real `ObsClient` against a `FakeObsServer`:
//! - the client reads cg OBS's scene transition at connect and again on cg
//!   OBS's transition events, and asks again until a read is answered;
//! - end to end with the real follow task on the client's snapshots: a scene
//!   whose playlist lookup failed (#218) is not followed, its repair is.
//!
//! Every wait is bounded (20 s); nothing sleeps for synchronisation.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use common::{FakeObsServer, FakeObsState};
use serde_json::{Value, json};
use sp_server::playback::program_bus::{ProgramBus, persist_and_cut};
use sp_server::playback::program_follow::{Follow, run_follow_task};
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
    pool: SqlitePool,
    fake: FakeObsServer,
    snapshots: watch::Receiver<obs::ObsSnapshot>,
    _rebuild: broadcast::Sender<()>,
    shutdown: broadcast::Sender<()>,
    client: obs::ObsClient,
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
            pool,
            fake,
            snapshots: client.snapshots(),
            _rebuild: rebuild,
            shutdown,
            client,
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
    assert_eq!(
        rig.transition_reads().await,
        3,
        "one read at connect and one per transition event, nothing else"
    );
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

#[tokio::test]
async fn the_follow_ignores_a_failed_lookup_and_follows_its_repair() {
    let mut rig = Rig::start(cg_obs()).await;
    db::models::set_setting(&rig.pool, "program_follow_obs", "true")
        .await
        .unwrap();
    let bus = Arc::new(ProgramBus::new());
    let (stop_follow, stop_rx) = broadcast::channel::<()>(1);
    let follow = tokio::spawn(run_follow_task(
        Follow::new(rig.pool.clone(), bus.clone()),
        rig.client.snapshots(),
        stop_rx,
        Duration::from_secs(3600),
    ));
    let source_becomes = |source: i64| {
        let bus = bus.clone();
        async move {
            let deadline = tokio::time::Instant::now() + TIMEOUT;
            while bus.status().source != Some(source) {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the program never cut to {source}"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    };
    // cg OBS shows sp-fast: the program follows it.
    source_becomes(7).await;
    // The operator takes the program elsewhere by hand.
    persist_and_cut(&rig.pool, &bus, 9).await.unwrap();
    // cg OBS switches to sp-slow and that scene's lookup gets no answer.
    rig.fake
        .update_state(|s| {
            s.drop_scene_item_lists = 1;
            s.program_scene = Some("sp-slow".into());
        })
        .await;
    rig.fake.push_program_scene_change("sp-slow").await;
    let failed = rig
        .snapshot_becomes("the failed lookup of sp-slow", |s| {
            s.lookup_failed.as_deref() == Some("sp-slow")
        })
        .await;
    assert!(
        failed.active_playlist_ids.contains(&7),
        "the client kept sp-fast's playlists: {failed:?}"
    );
    // The poll repairs the lookup; the follow cuts to sp-slow's playlist.
    source_becomes(8).await;
    let st = bus.status();
    assert_eq!(
        st.health.cuts, 3,
        "7, the manual 9, then 8 — never back to 7 on the kept playlists"
    );
    let _ = stop_follow.send(());
    tokio::time::timeout(TIMEOUT, follow)
        .await
        .expect("the follow task stops")
        .expect("the follow task did not panic");
    rig.stop().await;
}
