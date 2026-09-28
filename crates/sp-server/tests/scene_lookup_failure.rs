//! #218: a failed `GetSceneItemList` lookup is NOT an empty scene.
//!
//! The real `ObsClient` against a `FakeObsServer` whose next scene lookup
//! either gets no answer (the client's 2 s response timeout) or is answered
//! without a `sceneItems` list. Before the fix the client read either as
//! "this scene shows no playlist": it broadcast `SceneChanged { {} }` (the
//! engine bridge scene-offs, i.e. pauses, every playlist that was on
//! program) and the ~2 s poll, which compared scene names only, never
//! repaired the set. Now the failed lookup keeps the previous playlists and
//! broadcasts nothing, and the next poll looks the scene up again: the FIRST
//! `SceneChanged` after the failure already carries the right playlists.
//!
//! Every wait is bounded (20 s); nothing sleeps for synchronisation.

mod common;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use common::{FakeObsServer, FakeObsState};
use serde_json::json;
use sp_server::{db, obs};
use tokio::sync::{RwLock, broadcast};

const TIMEOUT: Duration = Duration::from_secs(20);

/// The real OBS client connected to a fake cg OBS whose program shows
/// `sp-fast` (playlist 7); `sp-slow` shows playlist 8.
struct Rig {
    fake: FakeObsServer,
    state: Arc<RwLock<obs::ObsState>>,
    events: broadcast::Receiver<obs::ObsEvent>,
    _rebuild: broadcast::Sender<()>,
    shutdown: broadcast::Sender<()>,
    _client: obs::ObsClient,
}

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
    s
}

fn set(ids: &[i64]) -> HashSet<i64> {
    ids.iter().copied().collect()
}

impl Rig {
    /// Connect the client to [`cg_obs`] and wait until it knows cg OBS's
    /// program: sp-fast showing playlist 7.
    async fn start() -> Self {
        Self::start_with(cg_obs()).await
    }

    /// [`Self::start`] against the fake cg OBS `cg`.
    async fn start_with(cg: FakeObsState) -> Self {
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
        let ndi_sources: obs::NdiSourceMap = Arc::new(RwLock::new(HashMap::new()));
        let state = Arc::new(RwLock::new(obs::ObsState::default()));
        let (event_tx, events) = broadcast::channel::<obs::ObsEvent>(64);
        let (rebuild, rebuild_rx) = broadcast::channel::<()>(4);
        let (shutdown, shutdown_rx) = broadcast::channel::<()>(1);
        let client = obs::ObsClient::spawn(
            obs::ObsConfig {
                url: fake.url(),
                password: None,
            },
            pool,
            ndi_sources,
            state.clone(),
            event_tx,
            rebuild_rx,
            shutdown_rx,
        );
        let mut rig = Self {
            fake,
            state,
            events,
            _rebuild: rebuild,
            shutdown,
            _client: client,
        };
        rig.program_becomes("sp-fast", &[7]).await;
        rig
    }

    /// Wait until the client's state shows `scene` with `playlists`.
    async fn program_becomes(&mut self, scene: &str, playlists: &[i64]) {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            {
                let s = self.state.read().await;
                if s.current_scene.as_deref() == Some(scene)
                    && s.active_playlist_ids == set(playlists)
                {
                    break;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the client never showed {scene} {playlists:?}: {:?}",
                self.state.read().await
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        while self.events.try_recv().is_ok() {}
    }

    /// The `GetSceneItemList` requests cg OBS received for `scene`.
    async fn lookups_of(&self, scene: &str) -> usize {
        self.fake
            .state()
            .await
            .requests
            .iter()
            .filter(|r| {
                r["requestType"] == "GetSceneItemList" && r["requestData"]["sceneName"] == scene
            })
            .count()
    }

    /// cg OBS puts `scene` on program (event included) while `fault` makes
    /// the next scene lookup fail. The FIRST `SceneChanged` the client
    /// broadcasts afterwards must already carry `playlists` (never an empty
    /// set in between, which the engine would read as "scene off"), and it
    /// must come from a second lookup: the poll's retry.
    async fn switch_with_a_failed_lookup(
        &mut self,
        scene: &str,
        fault: fn(&mut FakeObsState),
        playlists: &[i64],
    ) {
        while self.events.try_recv().is_ok() {}
        let before = self.lookups_of(scene).await;
        let program = scene.to_string();
        self.fake
            .update_state(move |s| {
                fault(s);
                s.program_scene = Some(program);
            })
            .await;
        self.fake.push_program_scene_change(scene).await;
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        let first = loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(left, self.events.recv()).await {
                Ok(Ok(obs::ObsEvent::SceneChanged {
                    scene_name,
                    active_playlist_ids,
                })) => break (scene_name, active_playlist_ids),
                Ok(Ok(_)) => continue,
                Ok(Err(e)) => panic!("event channel error: {e}"),
                Err(_) => panic!("no SceneChanged within {TIMEOUT:?} after the switch to {scene}"),
            }
        };
        assert_eq!(
            first,
            (scene.to_string(), set(playlists)),
            "the failed lookup of {scene} was read as a scene showing no playlist"
        );
        assert!(
            self.lookups_of(scene).await >= before + 2,
            "the failed lookup of {scene} and the poll's retry both reached cg OBS"
        );
        self.program_becomes(scene, playlists).await;
    }

    async fn stop(self) {
        let _ = self.shutdown.send(());
        self.fake.shutdown().await;
    }
}

fn drop_the_next_lookup(s: &mut FakeObsState) {
    s.drop_scene_item_lists = 1;
}

fn answer_the_next_lookup_without_items(s: &mut FakeObsState) {
    s.omit_scene_items = 1;
}

#[tokio::test]
async fn a_timed_out_scene_lookup_is_not_an_empty_scene_and_the_poll_repairs_it() {
    let mut rig = Rig::start().await;
    // cg OBS re-announces the scene already on program and its lookup times
    // out: nothing may scene-off (pause) playlist 7.
    rig.switch_with_a_failed_lookup("sp-fast", drop_the_next_lookup, &[7])
        .await;
    // cg OBS switches to sp-slow and that lookup times out: the poll looks
    // it up again and repairs the set to 8.
    rig.switch_with_a_failed_lookup("sp-slow", drop_the_next_lookup, &[8])
        .await;
    rig.stop().await;
}

#[tokio::test]
async fn a_scene_lookup_answered_without_scene_items_is_not_an_empty_scene() {
    let mut rig = Rig::start().await;
    rig.switch_with_a_failed_lookup("sp-fast", answer_the_next_lookup_without_items, &[7])
        .await;
    rig.switch_with_a_failed_lookup("sp-slow", answer_the_next_lookup_without_items, &[8])
        .await;
    rig.stop().await;
}

fn refuse_the_next_lookup(s: &mut FakeObsState) {
    s.refuse_scene_item_lists = 1;
}

#[tokio::test]
async fn a_refused_scene_lookup_is_a_failed_lookup_too() {
    let mut rig = Rig::start().await;
    rig.switch_with_a_failed_lookup("sp-slow", refuse_the_next_lookup, &[8])
        .await;
    rig.stop().await;
}

#[tokio::test]
async fn a_group_on_the_program_scene_is_refused_below_the_top_and_adds_nothing() {
    // obs-websocket 5 refuses `GetSceneItemList` for a group (602). A group
    // on sp-fast must neither fail sp-fast's lookup nor add a playlist: the
    // start (which waits for sp-fast = {7}) proves the lookup succeeded.
    let mut cg = cg_obs();
    cg.groups = vec!["Lower thirds".to_string()];
    cg.scene_items.get_mut("sp-fast").expect("sp-fast").push((
        "Lower thirds".to_string(),
        true,
        String::new(),
    ));
    let rig = Rig::start_with(cg).await;
    assert!(
        rig.lookups_of("Lower thirds").await >= 1,
        "the group was looked up and refused"
    );
    assert_eq!(rig.state.read().await.lookup_failed, None);
    rig.stop().await;
}
