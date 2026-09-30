//! #215: `SP-program` follows cg OBS and keeps the transition spec in step.
//! The settings, the telemetry, `apply_spec`, `follow_scene` over a real pool
//! and a `ProgramBus`, and how a snapshot of the OBS client reads (#219). The
//! task itself runs in `program_follow_tests_task.rs` and `FollowLoop`'s steps in
//! `program_follow_tests_loop.rs`; both share the helpers here (`pub(super)`).
//! Wired via `#[cfg(test)] #[path = "program_follow_tests.rs"] mod tests;`.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;

use super::*;
use crate::obs::ObsSnapshot;
use crate::playback::program_bus::{ProgramBus, SETTING_PROGRAM_SOURCE};
use crate::playback::program_transition::{
    ObsTransition, SpecSource, TransitionKind, TransitionMode, TransitionSpec,
};
use crate::remote::RemoteCut;
use crate::remote::map::{KeepReason, SceneAction};

pub(super) fn obs(name: &str, kind: &str, duration_ms: Option<u32>) -> ObsTransition {
    ObsTransition {
        name: name.to_string(),
        kind: kind.to_string(),
        duration_ms,
    }
}

/// cg OBS's transition: a Fade of `ms`.
pub(super) fn fade(ms: u32) -> ObsTransition {
    obs("Fade", "fade_transition", Some(ms))
}

/// cg OBS's transition: a Cut.
pub(super) fn cut() -> ObsTransition {
    obs("Cut", "cut_transition", None)
}

pub(super) fn set(ids: &[i64]) -> HashSet<i64> {
    ids.iter().copied().collect()
}

/// The OBS client's snapshot: connected, cg OBS on `scene` showing
/// `playlists`, looked up, transition unknown.
pub(super) fn on_program(scene: &str, playlists: &[i64]) -> ObsSnapshot {
    ObsSnapshot {
        connected: true,
        current_scene: Some(scene.to_string()),
        active_playlist_ids: set(playlists),
        lookup_failed: None,
        transition: None,
    }
}

/// The OBS client's snapshot after `scene`'s playlist lookup failed (#218):
/// the scene is named, `kept` are the previous scene's playlists.
pub(super) fn lookup_failed(scene: &str, kept: &[i64]) -> ObsSnapshot {
    ObsSnapshot {
        lookup_failed: Some(scene.to_string()),
        ..on_program(scene, kept)
    }
}

/// `snapshot` with cg OBS's `transition`.
pub(super) fn with(snapshot: ObsSnapshot, transition: ObsTransition) -> ObsSnapshot {
    ObsSnapshot {
        transition: Some(transition),
        ..snapshot
    }
}

pub(super) async fn pool() -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    pool
}

pub(super) async fn store(pool: &SqlitePool, key: &str, value: &str) {
    crate::db::models::set_setting(pool, key, value)
        .await
        .unwrap();
}

/// The last scene change the follow handled (`follow.last_follow_cut`).
pub(super) fn last_cut(bus: &ProgramBus) -> RemoteCut {
    bus.follow()
        .status(&FollowSettings::default())
        .last_follow_cut
        .expect("the follow is recorded")
}

/// The spec on the bus as `(kind, duration_ms, n_slots, source)`.
pub(super) fn spec_of(bus: &ProgramBus) -> (TransitionKind, u32, u32, SpecSource) {
    let t = bus.status().transition;
    (t.kind, t.duration_ms, t.n_slots, t.source)
}

#[test]
fn a_snapshot_knows_its_scene_only_connected_named_and_looked_up() {
    assert_eq!(
        program_scene(&on_program("sp-fast", &[7])),
        ProgramScene::Known(SceneView {
            scene: "sp-fast".to_string(),
            playlists: set(&[7]),
        })
    );
    assert_eq!(
        program_scene(&on_program("Slido", &[])),
        ProgramScene::Known(SceneView {
            scene: "Slido".to_string(),
            playlists: set(&[]),
        }),
        "a scene with no playlist is known (a manual scene)"
    );
    assert_eq!(
        program_scene(&lookup_failed("sp-slow", &[7])),
        ProgramScene::LookupFailed,
        "#218: the kept playlists belong to an earlier scene"
    );
    assert_eq!(
        program_scene(&ObsSnapshot::default()),
        ProgramScene::Unknown,
        "disconnected"
    );
    let unnamed = ObsSnapshot {
        connected: true,
        ..ObsSnapshot::default()
    };
    assert_eq!(
        program_scene(&unnamed),
        ProgramScene::Unknown,
        "no scene read yet"
    );
    let stale = ObsSnapshot {
        connected: false,
        ..on_program("sp-fast", &[7])
    };
    assert_eq!(
        program_scene(&stale),
        ProgramScene::Unknown,
        "not connected"
    );
    let gone_while_failed = ObsSnapshot {
        connected: false,
        ..lookup_failed("sp-slow", &[7])
    };
    assert_eq!(
        program_scene(&gone_while_failed),
        ProgramScene::Unknown,
        "not connected wins over a failed lookup: the reconnect is followed again"
    );
    assert_eq!(FOLLOW_SETTINGS_POLL, Duration::from_secs(5));
}

#[tokio::test]
async fn the_follow_settings_default_off_obs_300_and_load_trimmed() {
    let pool = pool().await;
    assert_eq!(
        FollowSettings::default(),
        FollowSettings {
            follow_obs: false,
            mode: TransitionMode::Obs,
            ms: 300,
        }
    );
    assert_eq!(
        load_follow_settings(&pool).await.unwrap(),
        FollowSettings::default()
    );
    store(&pool, "program_follow_obs", " true ").await;
    store(&pool, "program_transition", " fade ").await;
    store(&pool, "program_transition_ms", "450").await;
    let loaded = load_follow_settings(&pool).await.unwrap();
    assert_eq!(
        loaded,
        FollowSettings {
            follow_obs: true,
            mode: TransitionMode::Fade,
            ms: 450,
        }
    );
    store(&pool, "program_follow_obs", "True").await;
    assert!(
        !load_follow_settings(&pool).await.unwrap().follow_obs,
        "only \"true\" follows"
    );
    // An unreadable store keeps the settings in force.
    let follow = Follow::new(pool.clone(), Arc::new(ProgramBus::new()));
    pool.close().await;
    assert!(load_follow_settings(&pool).await.is_err());
    assert_eq!(follow.load(loaded).await, loaded);
}

#[test]
fn the_follow_telemetry_reports_the_stored_settings_and_what_it_saw() {
    let shared = FollowShared::default();
    assert_eq!(shared.obs_transition(), None);
    assert!(shared.set_obs_transition(obs("Fade", "fade_transition", Some(300))));
    assert!(
        !shared.set_obs_transition(obs("Fade", "fade_transition", Some(300))),
        "the same transition again is no change"
    );
    assert!(shared.set_obs_transition(obs("Fade", "fade_transition", Some(500))));
    let settings = FollowSettings {
        follow_obs: true,
        mode: TransitionMode::Cut,
        ms: 700,
    };
    assert_eq!(
        shared.status(&settings),
        FollowStatus {
            enabled: true,
            mode: TransitionMode::Cut,
            ms: 700,
            obs_transition: Some(obs("Fade", "fade_transition", Some(500))),
            last_follow_cut: None,
        }
    );
}

#[tokio::test]
async fn apply_spec_puts_the_override_or_cg_obs_transition_on_the_bus() {
    let bus = Arc::new(ProgramBus::new());
    let follow = Follow::new(pool().await, bus.clone());
    let obs_mode = FollowSettings::default();
    assert_eq!(
        follow.apply_spec(&obs_mode),
        TransitionSpec::fade(300, SpecSource::Fallback),
        "cg OBS's transition is not known yet: a fade of the setting's length"
    );
    assert_eq!(
        spec_of(&bus),
        (TransitionKind::Fade, 300, 9, SpecSource::Fallback)
    );
    bus.follow()
        .set_obs_transition(obs("Cut", "cut_transition", None));
    follow.apply_spec(&obs_mode);
    assert_eq!(spec_of(&bus), (TransitionKind::Cut, 0, 0, SpecSource::Obs));
    let fade_mode = FollowSettings {
        mode: TransitionMode::Fade,
        ms: 1000,
        ..obs_mode
    };
    follow.apply_spec(&fade_mode);
    assert_eq!(
        spec_of(&bus),
        (TransitionKind::Fade, 1000, 30, SpecSource::Setting),
        "the override wins over cg OBS"
    );
}

#[tokio::test]
async fn a_scene_showing_one_playlist_cuts_the_program_to_it_and_persists_it() {
    let pool = pool().await;
    let bus = Arc::new(ProgramBus::new());
    bus.select_initial(3, None);
    let follow = Follow::new(pool.clone(), bus.clone());
    assert_eq!(
        follow.follow_scene("sp-fast", &set(&[7])).await,
        SceneAction::Playlist(7)
    );
    let st = bus.status();
    assert_eq!((st.source, st.previous), (Some(7), Some(3)));
    assert_eq!(st.health.cuts, 1);
    assert_eq!(
        crate::db::models::get_setting(&pool, SETTING_PROGRAM_SOURCE)
            .await
            .unwrap()
            .as_deref(),
        Some("7")
    );
    let cut = last_cut(&bus);
    assert_eq!(
        (cut.scene.as_str(), cut.action, cut.source, cut.reason),
        ("sp-fast", "playlist", Some(7), None)
    );
    assert_eq!(cut.cut_boundary_100ns, st.cut_boundary_100ns);
    assert_eq!(
        bus.on_air_now().scene.as_deref(),
        Some("sp-fast"),
        "#221: the follow publishes the cg OBS scene it follows"
    );
    assert!(cut.cut_boundary_100ns.is_some());
    assert!(cut.at_ms > 0);
}

#[tokio::test]
async fn a_manual_scene_cuts_to_obs_manual_only_while_the_input_is_a_source() {
    let pool = pool().await;
    let bus = Arc::new(ProgramBus::new());
    bus.select_initial(3, None);
    let follow = Follow::new(pool.clone(), bus.clone());
    // The input is off: a manual (or multi-playlist) scene keeps the program.
    assert_eq!(
        follow.follow_scene("Slido", &set(&[3, 7])).await,
        SceneAction::Keep(KeepReason::InputInactive)
    );
    assert_eq!(bus.status().source, Some(3));
    assert_eq!(bus.status().health.cuts, 0);
    let kept = last_cut(&bus);
    assert_eq!(
        (
            kept.action,
            kept.source,
            kept.reason,
            kept.cut_boundary_100ns
        ),
        ("keep", None, Some("input_inactive"), None)
    );
    assert_eq!(
        crate::db::models::get_setting(&pool, SETTING_PROGRAM_SOURCE)
            .await
            .unwrap(),
        None,
        "nothing persisted"
    );
    // The input is a source: the same scene cuts to "OBS manuál" (-1).
    store(&pool, "ndi_input_enabled", "true").await;
    store(&pool, "ndi_input_source", "CG-OBS (manual)").await;
    assert_eq!(
        follow.follow_scene("Slido", &set(&[])).await,
        SceneAction::Input
    );
    assert_eq!(bus.status().source, Some(-1));
    let input = last_cut(&bus);
    assert_eq!(
        (input.action, input.source, input.reason),
        ("input", Some(-1), None)
    );
}

#[tokio::test]
async fn a_scene_already_on_program_cuts_nothing_and_a_failed_persist_cuts_nothing() {
    let pool = pool().await;
    let bus = Arc::new(ProgramBus::new());
    bus.select_initial(7, None);
    let follow = Follow::new(pool.clone(), bus.clone());
    assert_eq!(
        follow.follow_scene("sp-fast", &set(&[7])).await,
        SceneAction::Playlist(7)
    );
    assert_eq!(bus.status().health.cuts, 0, "it already shows 7");
    assert_eq!(
        crate::db::models::get_setting(&pool, SETTING_PROGRAM_SOURCE)
            .await
            .unwrap(),
        None,
        "nothing persisted"
    );
    let same = last_cut(&bus);
    assert_eq!(
        (same.source, same.reason, same.cut_boundary_100ns),
        (Some(7), None, None)
    );

    // The store is gone: the source cannot be persisted, so nothing is cut.
    pool.close().await;
    assert_eq!(
        follow.follow_scene("sp-slow", &set(&[8])).await,
        SceneAction::Playlist(8)
    );
    assert_eq!(bus.status().source, Some(7));
    assert_eq!(bus.status().health.cuts, 0);
    let failed = last_cut(&bus);
    assert_eq!(
        (failed.scene.as_str(), failed.source, failed.reason),
        ("sp-slow", Some(8), Some("persist_failed"))
    );
}

// ── #221 🟡 1: the follow records what cg OBS shows ────────────────────────

/// An active playlist `pid` whose NDI output is `ndi_name` (the scene catalog
/// names its scene by that, lowercased).
async fn playlist(pool: &SqlitePool, pid: i64, ndi_name: &str) {
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (?, ?, 'u', ?, 1)",
    )
    .bind(pid)
    .bind(format!("playlist {pid}"))
    .bind(ndi_name)
    .execute(pool)
    .await
    .unwrap();
}

/// SongPlayer's record says cg OBS shows `shown` (an answered command).
fn record_shown(bus: &ProgramBus, shown: Option<i64>) {
    let legacy = bus.legacy_cg();
    let ticket = legacy.ticket();
    assert!(legacy.confirmed(ticket, shown));
}

/// The playlists on air now (`program_on_air::on_air_set`), ascending.
fn on_air(bus: &ProgramBus) -> Vec<i64> {
    let shown = bus.legacy_cg().shown_now();
    crate::playback::program_on_air::on_air_set(&bus.on_air_now(), shown)
        .into_iter()
        .collect()
}

/// Release 0.69.0 review 🟡 1: with the follow on (the facade rollback, or
/// the settings checkbox), cg OBS's program scene IS what cg OBS shows. The
/// follow records it in `legacy_cg` like the switch path records a mirror:
/// a playlist scene of the catalog → that playlist, a manual scene → none,
/// on every known scene. Before, `shown` kept the last mirrored playlist,
/// and it stayed on air next to the followed one for good.
#[tokio::test]
async fn the_follow_records_what_cg_obs_shows() {
    let pool = pool().await;
    playlist(&pool, 3, "SP-three").await;
    playlist(&pool, 7, "SP-seven").await;
    let bus = Arc::new(ProgramBus::new());
    bus.select_initial(7, Some("sp-seven"));
    record_shown(&bus, Some(7));
    let follow = Follow::new(pool.clone(), bus.clone());

    follow.follow_scene("sp-three", &set(&[3])).await;
    assert_eq!(bus.status().source, Some(3));
    assert_eq!(bus.legacy_cg().shown_now(), Some(3), "cg OBS shows 3");
    assert_eq!(on_air(&bus), vec![3], "7 left the air");

    // A manual scene: cg OBS shows no playlist. The input is off, so the
    // program keeps 3, and 3 alone is on air.
    follow.follow_scene("Slido", &set(&[])).await;
    assert_eq!(bus.status().source, Some(3));
    assert_eq!(bus.legacy_cg().shown_now(), None, "a manual scene");
    assert_eq!(on_air(&bus), vec![3]);

    // A scene the program already shows is recorded too (the catalog's
    // scene, whatever ASCII case cg OBS names it in).
    record_shown(&bus, Some(7));
    follow.follow_scene("SP-Three", &set(&[3])).await;
    assert_eq!(bus.status().health.cuts, 1, "3 was on program already");
    assert_eq!(bus.legacy_cg().shown_now(), Some(3));
    assert_eq!(on_air(&bus), vec![3]);
}

/// The follow's record is ticketed under the switch order, like the switch
/// path's: it waits for a switch in progress, and a late answer to a
/// command sent before it never overwrites what it recorded.
#[tokio::test]
async fn the_follow_waits_for_the_switch_order_and_outranks_an_older_command() {
    let pool = pool().await;
    playlist(&pool, 3, "SP-three").await;
    playlist(&pool, 7, "SP-seven").await;
    let bus = Arc::new(ProgramBus::new());
    bus.select_initial(7, Some("sp-seven"));
    // A switch is in progress; its mirror (to 7) is not answered yet.
    let order = bus.switch_order().lock().await;
    let older = bus.legacy_cg().ticket();
    let follow = Follow::new(pool.clone(), bus.clone());
    let task = tokio::spawn(async move { follow.follow_scene("sp-three", &set(&[3])).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        bus.status().source,
        Some(7),
        "the follow waits for the switch"
    );
    assert_eq!(bus.legacy_cg().shown_now(), None);
    drop(order);
    assert_eq!(task.await.unwrap(), SceneAction::Playlist(3));
    assert_eq!(bus.legacy_cg().shown_now(), Some(3));
    assert!(
        !bus.legacy_cg().confirmed(older, Some(7)),
        "the older command's late answer is dropped"
    );
    assert_eq!(bus.legacy_cg().shown_now(), Some(3));
}
