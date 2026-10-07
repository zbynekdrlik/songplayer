//! #233: #210's three VBAN keys become one entry per target — 48 kHz fixed,
//! INT24, the same wire stream name — once, while no list is stored; the old
//! keys stay for a rollback (main-session ruling 4).

use super::*;
use sp_core::audio_outputs::{RateChoice, VbanSampleFormat};

fn snv() -> Migrated {
    entries_from_vban(true, "sp-program", "fohabl.lan:6980,lv1.lan:6980")
}

#[test]
fn snv_keeps_both_targets_at_48k_int24_sp_program() {
    let m = snv();
    assert!(m.skipped.is_empty());
    assert_eq!(m.entries.len(), 2);
    let foh = &m.entries[0];
    assert_eq!(
        (foh.id.as_str(), foh.name.as_str()),
        ("out-1", "fohabl.lan:6980")
    );
    assert!(foh.enabled);
    assert_eq!(foh.rate, RateChoice::Fixed(48_000), "never \"network\"");
    assert_eq!(foh.delay_ms, 0);
    let v = foh.vban.as_ref().unwrap();
    assert_eq!((v.host.as_str(), v.port), ("fohabl.lan", 6980));
    assert_eq!(v.stream_name, "sp-program");
    assert_eq!(v.format, VbanSampleFormat::Int24);
    assert_eq!(m.entries[1].id, "out-2");
    assert_eq!(m.entries[1].name, "lv1.lan:6980");
    assert_eq!(m.entries[1].vban.as_ref().unwrap().host, "lv1.lan");
    assert_eq!(m.entries[1].rate, RateChoice::Fixed(48_000));
}

#[test]
fn the_stream_name_is_the_wire_name_210_sent() {
    let blank = entries_from_vban(true, "  ", "h:1");
    assert_eq!(
        blank.entries[0].vban.as_ref().unwrap().stream_name,
        "sp-program"
    );
    let odd = entries_from_vban(true, " čo-je-to-za-stream ", "h:1");
    assert_eq!(
        odd.entries[0].vban.as_ref().unwrap().stream_name,
        "_o-je-to-za-stre"
    );
}

#[test]
fn enabled_only_when_210_was_enabled() {
    assert!(!entries_from_vban(false, "", "h:1").entries[0].enabled);
    assert!(entries_from_vban(true, "", "h:1").entries[0].enabled);
}

#[test]
fn no_target_is_an_empty_list() {
    assert_eq!(entries_from_vban(true, "x", " , ,"), Migrated::default());
    assert_eq!(entries_from_vban(true, "x", ""), Migrated::default());
}

#[test]
fn the_first_8_targets_migrate_like_210_used_them() {
    let ten: Vec<String> = (1..=10).map(|n| format!("h{n}:6980")).collect();
    let m = entries_from_vban(true, "", &ten.join(", "));
    assert_eq!(m.entries.len(), 8);
    assert_eq!(m.entries[7].id, "out-8");
    assert_eq!(m.entries[7].vban.as_ref().unwrap().host, "h8");
    assert_eq!(
        m.skipped,
        vec![
            "h9:6980: over the 8 targets #210 used".to_string(),
            "h10:6980: over the 8 targets #210 used".to_string(),
        ]
    );
}

#[test]
fn a_target_210_never_resolved_is_skipped_and_named() {
    let m = entries_from_vban(true, "", "nohost, :6980, h:0, h:x, ok:1, a b:2");
    assert_eq!(m.entries.len(), 1);
    assert_eq!(m.entries[0].id, "out-1");
    assert_eq!(m.entries[0].vban.as_ref().unwrap().host, "ok");
    assert_eq!(
        m.skipped,
        vec![
            "nohost: not host:port".to_string(),
            ":6980: not host:port".to_string(),
            "h:0: not host:port".to_string(),
            "h:x: not host:port".to_string(),
            "a b:2: entry 2 (id out-2): vban.host has a character that is not allowed".to_string(),
        ]
    );
}

#[test]
fn split_target_takes_the_last_colon() {
    assert_eq!(
        split_target("fohabl.lan:6980"),
        Some(("fohabl.lan".into(), 6980))
    );
    assert_eq!(split_target(" h : 1 "), Some(("h".into(), 1)));
    assert_eq!(split_target("h:65535"), Some(("h".into(), 65535)));
    assert_eq!(split_target("a:b:7"), Some(("a:b".into(), 7)));
    assert_eq!(split_target("h:65536"), None);
    assert_eq!(split_target("h:0"), None);
    assert_eq!(split_target("h"), None);
    assert_eq!(split_target(":1"), None);
}

async fn pool() -> sqlx::SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    pool
}

async fn set(pool: &sqlx::SqlitePool, k: &str, v: &str) {
    crate::db::models::set_setting(pool, k, v).await.unwrap();
}

async fn get(pool: &sqlx::SqlitePool, k: &str) -> Option<String> {
    crate::db::models::get_setting(pool, k).await.unwrap()
}

#[tokio::test]
async fn the_first_start_writes_the_list_once_and_keeps_the_old_keys() {
    let pool = pool().await;
    set(&pool, "vban_enabled", "true").await;
    set(&pool, "vban_stream_name", "sp-program").await;
    set(&pool, "vban_targets", "fohabl.lan:6980,lv1.lan:6980").await;
    let outcome = migrate_vban_settings(&pool).await.unwrap();
    assert_eq!(outcome, MigrationOutcome::Migrated(snv()));
    let list = get(&pool, "audio_outputs").await.unwrap();
    let parsed = crate::playback::audio_out_config::parse_list(&list).unwrap();
    assert_eq!(
        parsed,
        snv().entries,
        "the stored list parses back strictly"
    );
    // Ruling 4: a rollback to <= 0.72 still finds #210's keys.
    assert_eq!(get(&pool, "vban_enabled").await.as_deref(), Some("true"));
    assert_eq!(
        get(&pool, "vban_stream_name").await.as_deref(),
        Some("sp-program")
    );
    assert_eq!(
        get(&pool, "vban_targets").await.as_deref(),
        Some("fohabl.lan:6980,lv1.lan:6980")
    );
    assert_eq!(
        migrate_vban_settings(&pool).await.unwrap(),
        MigrationOutcome::Nothing
    );
    assert_eq!(
        get(&pool, "audio_outputs").await.unwrap(),
        list,
        "untouched the second time"
    );
}

#[tokio::test]
async fn a_stored_list_wins_and_the_old_keys_stay_untouched() {
    let pool = pool().await;
    set(&pool, "audio_outputs", "[]").await;
    set(&pool, "vban_targets", "h:1").await;
    set(&pool, "vban_enabled", "true").await;
    assert_eq!(
        migrate_vban_settings(&pool).await.unwrap(),
        MigrationOutcome::Nothing
    );
    assert_eq!(get(&pool, "audio_outputs").await.unwrap(), "[]");
    assert_eq!(get(&pool, "vban_targets").await.as_deref(), Some("h:1"));
    assert_eq!(get(&pool, "vban_enabled").await.as_deref(), Some("true"));
}

#[tokio::test]
async fn a_box_that_never_had_vban_gets_nothing_written() {
    let pool = pool().await;
    assert_eq!(
        migrate_vban_settings(&pool).await.unwrap(),
        MigrationOutcome::Nothing
    );
    assert_eq!(get(&pool, "audio_outputs").await, None);
}

#[tokio::test]
async fn one_old_key_alone_still_migrates() {
    // Targets with no `vban_enabled` row: #210 sent nothing (only "true"
    // enables), so the entries are kept, switched off.
    let pool = pool().await;
    set(&pool, "vban_targets", "h:1").await;
    let MigrationOutcome::Migrated(m) = migrate_vban_settings(&pool).await.unwrap() else {
        panic!("migrated");
    };
    assert_eq!(m.entries.len(), 1);
    assert!(!m.entries[0].enabled);
    assert_eq!(
        m.entries[0].vban.as_ref().unwrap().stream_name,
        "sp-program"
    );
    // Only the enabled key: an empty list is stored, so it never runs again.
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    set(&pool, "vban_enabled", " true ").await;
    assert_eq!(
        migrate_vban_settings(&pool).await.unwrap(),
        MigrationOutcome::Migrated(Migrated::default())
    );
    assert_eq!(get(&pool, "audio_outputs").await.unwrap(), "[]");
}

#[tokio::test]
async fn a_list_written_meanwhile_is_never_replaced() {
    let pool = pool().await;
    assert!(store_list_if_absent(&pool, "[1]").await.unwrap());
    assert_eq!(get(&pool, "audio_outputs").await.unwrap(), "[1]");
    assert!(!store_list_if_absent(&pool, "[2]").await.unwrap());
    assert_eq!(get(&pool, "audio_outputs").await.unwrap(), "[1]");
}
