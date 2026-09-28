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
        let mut rig = Self::connect(cg).await;
        rig.program_becomes("sp-fast", &[7]).await;
        rig
    }

    /// Spawn the client against `cg` without waiting for anything; `events`
    /// holds every event from the very start.
    async fn connect(cg: FakeObsState) -> Self {
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
        Self {
            fake,
            state,
            events,
            _rebuild: rebuild,
            shutdown,
            _client: client,
        }
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

    /// Wait (at most 20 s) until the client's state satisfies `done`. Checks
    /// every 2 ms: a held answer (below) must be released well inside the
    /// client's 2 s response timeout.
    async fn until(&self, what: &str, done: impl Fn(&obs::ObsState) -> bool) {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        while !done(&self.state.read().await) {
            assert!(tokio::time::Instant::now() < deadline, "never: {what}");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// Wait (at most 20 s) until the fake cg OBS's state satisfies `done`
    /// (every 2 ms, as [`Self::until`]).
    async fn fake_until(&self, what: &str, done: impl Fn(&FakeObsState) -> bool) {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        while !done(&self.fake.state().await) {
            assert!(tokio::time::Instant::now() < deadline, "never: {what}");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
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

/// Review round 2: a poll whose `GetCurrentProgramScene` answered BEFORE an
/// event overtook it must never outrank that event's apply. Here the poll
/// reads sp-fast (whose lookup keeps failing, so it looks sp-fast up again),
/// cg OBS then switches to sp-slow, and sp-slow's lookup answers after the
/// poll's stale relookup. The client must end on sp-slow from THAT answer.
/// sp-slow's later lookups stay held, so only the event's answer can get it
/// there (a reconcile never answers).
#[tokio::test]
async fn a_poll_read_an_event_overtook_never_rolls_the_scene_back() {
    let mut rig = Rig::start().await;
    // sp-fast's lookups are refused (a sticky failure): the poll relooks it up.
    rig.fake
        .update_state(|s| s.groups = vec!["sp-fast".to_string()])
        .await;
    rig.fake.push_program_scene_change("sp-fast").await;
    rig.until("sp-fast's lookup failed", |s| {
        s.lookup_failed.as_deref() == Some("sp-fast")
    })
    .await;
    // The poll's next read of cg OBS's program answers sp-fast, held.
    rig.fake.update_state(|s| s.hold_program_scene = true).await;
    rig.fake_until("a held program read", |s| {
        s.held
            .iter()
            .any(|r| r["d"]["requestType"] == "GetCurrentProgramScene")
    })
    .await;
    // cg OBS switches to sp-slow; its event overtakes that read, and its
    // lookup is held. sp-fast's lookups answer again from now on.
    rig.fake
        .update_state(|s| {
            s.groups.clear();
            s.program_scene = Some("sp-slow".to_string());
            s.hold_lookups_of = Some("sp-slow".to_string());
        })
        .await;
    rig.fake.push_program_scene_change("sp-slow").await;
    rig.fake_until("sp-slow's held lookup", |s| {
        s.held
            .iter()
            .any(|r| r["d"]["requestType"] == "GetSceneItemList")
    })
    .await;
    // The stale read answers sp-fast: the poll looks sp-fast up again (it
    // answers now). Then sp-slow's lookup answers. Whichever of the two
    // lands first, the event's must win. (Every hold here is well under the
    // client's 2 s response timeout.)
    let asked = rig.lookups_of("sp-fast").await;
    rig.fake
        .update_state(|s| s.hold_program_scene = false)
        .await;
    rig.fake.release_held("GetCurrentProgramScene").await;
    rig.fake_until("the stale relookup of sp-fast", move |s| {
        s.requests
            .iter()
            .filter(|r| {
                r["requestType"] == "GetSceneItemList" && r["requestData"]["sceneName"] == "sp-fast"
            })
            .count()
            > asked
    })
    .await;
    rig.fake.release_held("GetSceneItemList").await;
    rig.program_becomes("sp-slow", &[8]).await;
    rig.stop().await;
}

/// Review round 3: an event cg OBS sent while the client was still
/// connecting (read during its NDI map rebuild, queued until the connection
/// loop runs) is OLDER than the client's initial program read. It must never
/// override that read: cg OBS shows sp-fast, so the only `SceneChanged` is
/// sp-fast's — no sp-slow after it (the engine would scene-off sp-fast and the
/// follow would cut to sp-slow).
#[tokio::test]
async fn an_event_queued_during_the_connect_never_overrides_the_initial_read() {
    let mut cg = cg_obs(); // cg OBS shows sp-fast
    cg.event_on_input_list = Some("sp-slow".to_string());
    let mut rig = Rig::connect(cg).await;
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let mut changes = Vec::new();
    // The initial read and two polls (the first poll runs right after it).
    loop {
        while let Ok(event) = rig.events.try_recv() {
            if let obs::ObsEvent::SceneChanged {
                scene_name,
                active_playlist_ids,
            } = event
            {
                changes.push((scene_name, active_playlist_ids));
            }
        }
        let reads = rig
            .fake
            .state()
            .await
            .requests
            .iter()
            .filter(|r| r["requestType"] == "GetCurrentProgramScene")
            .count();
        if reads >= 3 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the client never polled twice after its initial read"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    while let Ok(event) = rig.events.try_recv() {
        if let obs::ObsEvent::SceneChanged {
            scene_name,
            active_playlist_ids,
        } = event
        {
            changes.push((scene_name, active_playlist_ids));
        }
    }
    assert_eq!(
        changes,
        vec![("sp-fast".to_string(), set(&[7]))],
        "the queued, older sp-slow event overrode the initial read"
    );
    let s = rig.state.read().await;
    assert_eq!(
        (s.current_scene.as_deref(), &s.active_playlist_ids),
        (Some("sp-fast"), &set(&[7]))
    );
    drop(s);
    rig.stop().await;
}
