//! #221 L1: what is on air — the resolver, and the program bus's on-air
//! publication (every cut, the startup selection, `persist_and_cut`, the
//! restore). Wired via `#[cfg(test)] #[path = "program_on_air_tests.rs"] mod
//! tests;`.

use std::collections::BTreeSet;

use sp_core::config::{PROGRAM_INPUT_ID, PROGRAM_INPUT_LABEL};
use sqlx::SqlitePool;

use super::*;
use crate::playback::program_bus::{
    ProgramBus, SETTING_PROGRAM_SOURCE, persist_and_cut, restore_selected_source,
};
use crate::playback::wallclock::utc_now_100ns;

fn on_air(seq: u64, source: Option<i64>, scene: Option<&str>) -> OnAir {
    OnAir {
        seq,
        source,
        scene: scene.map(str::to_string),
    }
}

#[test]
fn the_resolver_names_the_scene_else_the_input_else_nothing() {
    assert_eq!(
        program_scene_name(&on_air(3, Some(7), Some("sp-fast"))).as_deref(),
        Some("sp-fast")
    );
    assert_eq!(
        program_scene_name(&on_air(4, Some(PROGRAM_INPUT_ID), Some("Slido"))).as_deref(),
        Some("Slido"),
        "a manual press names the cg OBS scene the input carries"
    );
    assert_eq!(
        program_scene_name(&on_air(5, Some(PROGRAM_INPUT_ID), None)).as_deref(),
        Some(PROGRAM_INPUT_LABEL)
    );
    assert_eq!(PROGRAM_INPUT_LABEL, "OBS manuál");
    assert_eq!(
        program_scene_name(&on_air(6, Some(7), None)),
        None,
        "a playlist whose catalog names no scene"
    );
    assert_eq!(
        program_scene_name(&OnAir::default()),
        None,
        "nothing on air"
    );
}

#[test]
fn every_publication_counts_up_and_carries_the_source_and_the_scene() {
    let first = OnAir::default().next(7, Some("sp-fast"));
    assert_eq!(first, on_air(1, Some(7), Some("sp-fast")));
    let second = first.next(PROGRAM_INPUT_ID, None);
    assert_eq!(second, on_air(2, Some(PROGRAM_INPUT_ID), None));
    assert_eq!(second.next(7, Some("sp-fast")).seq, 3);
}

#[test]
fn a_fresh_bus_has_nothing_on_air() {
    let bus = ProgramBus::new();
    assert_eq!(bus.on_air_now(), OnAir::default());
    assert_eq!(*bus.on_air().borrow(), OnAir::default());
}

#[test]
fn the_startup_selection_is_published_before_anyone_subscribes() {
    // `restore_selected_source` runs before any task subscribes: a `send`
    // with no receiver would drop the value.
    let bus = ProgramBus::new();
    bus.select_initial(5, Some("sp-90s"));
    let rx = bus.on_air();
    assert_eq!(*rx.borrow(), on_air(1, Some(5), Some("sp-90s")));
    assert_eq!(bus.on_air_now(), on_air(1, Some(5), Some("sp-90s")));
    assert_eq!(bus.status().source, Some(5));
}

#[test]
fn every_cut_is_published_a_cut_to_the_source_on_air_too() {
    let bus = ProgramBus::new();
    bus.select_initial(3, Some("sp-slow"));
    let mut rx = bus.on_air();
    assert!(
        !rx.has_changed().unwrap(),
        "a new receiver has seen the value"
    );
    let now = utc_now_100ns();
    let st = bus.cut(7, now, Some("sp-fast"));
    assert_eq!((st.source, st.previous), (Some(7), Some(3)));
    assert!(rx.has_changed().unwrap());
    assert_eq!(*rx.borrow_and_update(), on_air(2, Some(7), Some("sp-fast")));
    // The same source again: the bus records nothing, the publication counts.
    let st = bus.cut(7, now, Some("sp-fast"));
    assert_eq!(
        st.health.cuts, 1,
        "a cut to the selected source is a bus no-op"
    );
    assert!(rx.has_changed().unwrap(), "yet it is published");
    assert_eq!(*rx.borrow_and_update(), on_air(3, Some(7), Some("sp-fast")));
}

#[test]
fn a_manual_to_manual_cut_keeps_the_input_and_names_the_new_scene() {
    let bus = ProgramBus::new();
    bus.select_initial(PROGRAM_INPUT_ID, Some("Slido"));
    let st = bus.cut(PROGRAM_INPUT_ID, utc_now_100ns(), Some("Trailer"));
    assert_eq!(st.source, Some(PROGRAM_INPUT_ID));
    assert_eq!(st.health.cuts, 0, "no mix: the input stays on air");
    let now = bus.on_air_now();
    assert_eq!(now, on_air(2, Some(PROGRAM_INPUT_ID), Some("Trailer")));
    assert_eq!(program_scene_name(&now).as_deref(), Some("Trailer"));
}

async fn pool() -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    pool
}

async fn add_playlist(pool: &SqlitePool, id: i64, ndi: &str) {
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active)
         VALUES (?, ?, ?, ?, 1)",
    )
    .bind(id)
    .bind(format!("p{id}"))
    .bind(format!("https://youtube.com/playlist?list=p{id}"))
    .bind(ndi)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn persist_and_cut_publishes_the_scene_it_was_given() {
    let pool = pool().await;
    let bus = ProgramBus::new();
    let st = persist_and_cut(&pool, &bus, 7, Some("sp-fast"))
        .await
        .unwrap();
    assert_eq!(st.source, Some(7));
    assert_eq!(bus.on_air_now(), on_air(1, Some(7), Some("sp-fast")));
    let persisted = crate::db::models::get_setting(&pool, SETTING_PROGRAM_SOURCE)
        .await
        .unwrap();
    assert_eq!(persisted.as_deref(), Some("7"));
}

#[tokio::test]
async fn the_restore_publishes_the_playlists_catalog_scene() {
    let pool = pool().await;
    add_playlist(&pool, 5, "SP-90s").await;
    add_playlist(&pool, 7, "SP-fast").await;
    crate::db::models::set_setting(&pool, SETTING_PROGRAM_SOURCE, "5")
        .await
        .unwrap();
    let bus = ProgramBus::new();
    assert_eq!(restore_selected_source(&pool, &bus).await, Some(5));
    assert_eq!(bus.on_air_now(), on_air(1, Some(5), Some("sp-90s")));
}

#[tokio::test]
async fn a_restored_input_is_named_by_the_resolver() {
    let pool = pool().await;
    for (key, value) in [
        ("ndi_input_enabled", "true"),
        ("ndi_input_source", "CG-OBS (manual)"),
        (SETTING_PROGRAM_SOURCE, "-1"),
    ] {
        crate::db::models::set_setting(&pool, key, value)
            .await
            .unwrap();
    }
    let bus = ProgramBus::new();
    assert_eq!(
        restore_selected_source(&pool, &bus).await,
        Some(PROGRAM_INPUT_ID)
    );
    let now = bus.on_air_now();
    assert_eq!(now, on_air(1, Some(PROGRAM_INPUT_ID), None));
    assert_eq!(program_scene_name(&now).as_deref(), Some("OBS manuál"));
}

fn set(pids: &[i64]) -> BTreeSet<i64> {
    pids.iter().copied().collect()
}

/// #221 L4b §1e: on air = SP-program's playlist ∪ the playlist SongPlayer
/// last told cg OBS to show. "OBS manuál" (-1) is no playlist.
#[test]
fn on_air_is_the_program_s_playlist_and_what_cg_obs_was_told() {
    let fast = on_air(2, Some(7), Some("sp-fast"));
    assert_eq!(on_air_set(&fast, Some(7)), set(&[7]), "they agree");
    assert_eq!(
        on_air_set(&fast, Some(4)),
        set(&[4, 7]),
        "cg OBS still shows sp-slow (its mirror is unanswered or failed)"
    );
    assert_eq!(
        on_air_set(&fast, None),
        set(&[7]),
        "cg OBS shows a manual scene"
    );
    let manual = on_air(3, Some(PROGRAM_INPUT_ID), Some("Slido"));
    assert_eq!(on_air_set(&manual, None), set(&[]));
    assert_eq!(
        on_air_set(&manual, Some(7)),
        set(&[7]),
        "a dashboard cut to OBS manuál while cg OBS shows sp-fast: the input carries it"
    );
    assert_eq!(on_air_set(&OnAir::default(), None), set(&[]), "nothing yet");
    assert_eq!(on_air_set(&OnAir::default(), Some(4)), set(&[4]));
}

/// #221 L4b: OFF for every playlist that left, then ON for every playlist
/// that entered and for the source just cut to (the re-kick), each part
/// ascending. Review round 1: a member nobody cut to is never re-kicked.
#[test]
fn a_change_is_off_for_what_left_then_on_for_what_entered_and_the_cut_source() {
    assert!(on_air_changes(&set(&[]), &set(&[]), None).is_empty());
    assert_eq!(
        on_air_changes(&set(&[]), &set(&[7]), None),
        vec![(7, true)],
        "the restored program"
    );
    assert_eq!(
        on_air_changes(&set(&[7]), &set(&[4, 7]), Some(4)),
        vec![(4, true)],
        "a press p→q: q only; p (cg OBS still shows it) is not re-kicked"
    );
    assert_eq!(
        on_air_changes(&set(&[7]), &set(&[4, 7]), None),
        vec![(4, true)],
        "a playlist that entered without a cut"
    );
    assert_eq!(
        on_air_changes(&set(&[4, 7]), &set(&[4]), None),
        vec![(7, false)],
        "cg OBS confirmed the press: off only"
    );
    assert_eq!(
        on_air_changes(&set(&[4, 7]), &set(&[4]), Some(4)),
        vec![(7, false), (4, true)],
        "off first"
    );
    assert_eq!(
        on_air_changes(&set(&[4]), &set(&[4]), Some(4)),
        vec![(4, true)],
        "a press of the scene already on air: the re-kick"
    );
    assert!(
        on_air_changes(&set(&[4]), &set(&[4]), None).is_empty(),
        "no cut: nothing re-kicked"
    );
    assert!(
        on_air_changes(&set(&[4]), &set(&[4]), Some(-1)).is_empty(),
        "a cut to OBS manuál while cg OBS shows 4: 4 is not re-kicked"
    );
    assert!(
        on_air_changes(&set(&[4]), &set(&[4]), Some(9)).is_empty(),
        "a cut source that is not on air is never ON"
    );
    assert_eq!(
        on_air_changes(&set(&[2, 9, 4]), &set(&[5, 3]), Some(5)),
        vec![(2, false), (4, false), (9, false), (3, true), (5, true)]
    );
    assert_eq!(
        on_air_changes(&set(&[4]), &set(&[]), None),
        vec![(4, false)]
    );
}
