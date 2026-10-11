//! #223 S12a: the worker's gate, pick, counts and one tick with scripted
//! steps (the shared `test_rig`).

use std::sync::Mutex;

use super::super::test_rig::*;
use super::*;

// ---- decide ---------------------------------------------------------------

const T: i64 = 1_760_000_000_000;

fn open_gate() -> Gate {
    Gate {
        enabled: true,
        held: false,
        download_due: false,
        paused_until_ms: None,
        last_start_ms: None,
        free_bytes: Some(MIN_FREE_BYTES),
        now_ms: T,
    }
}

#[test]
fn an_open_gate_lets_a_tick_pick() {
    assert_eq!(decide(&open_gate()), Ok(()));
}

/// Each closed gate names itself; the switch is asked first, the disk last.
#[test]
fn each_gate_skips_with_its_reason_in_order() {
    let all_closed = Gate {
        enabled: false,
        held: true,
        download_due: true,
        paused_until_ms: Some(T + 1),
        last_start_ms: Some(T),
        free_bytes: None,
        now_ms: T,
    };
    assert_eq!(decide(&all_closed), Err(Skip::Off));
    let g = Gate {
        enabled: true,
        ..all_closed
    };
    assert_eq!(decide(&g), Err(Skip::Held));
    let g = Gate { held: false, ..g };
    assert_eq!(decide(&g), Err(Skip::DownloadDue));
    let g = Gate {
        download_due: false,
        ..g
    };
    assert_eq!(decide(&g), Err(Skip::Paused));
    let g = Gate {
        paused_until_ms: None,
        ..g
    };
    assert_eq!(decide(&g), Err(Skip::Spacing));
    let g = Gate {
        last_start_ms: None,
        ..g
    };
    assert_eq!(decide(&g), Err(Skip::NoDiskReading));
}

#[test]
fn a_pause_ends_at_its_instant() {
    let g = |until| Gate {
        paused_until_ms: Some(until),
        ..open_gate()
    };
    assert_eq!(decide(&g(T + 1)), Err(Skip::Paused));
    assert_eq!(decide(&g(T)), Ok(()));
}

#[test]
fn two_starts_are_at_least_two_minutes_apart() {
    let g = |start| Gate {
        last_start_ms: Some(start),
        ..open_gate()
    };
    assert_eq!(decide(&g(T - 119_999)), Err(Skip::Spacing));
    assert_eq!(decide(&g(T - 120_000)), Ok(()));
}

#[test]
fn the_disk_floor_is_50_gib() {
    assert_eq!(MIN_FREE_BYTES, 50 * 1024 * 1024 * 1024);
    let g = |free| Gate {
        free_bytes: Some(free),
        ..open_gate()
    };
    assert_eq!(decide(&g(MIN_FREE_BYTES - 1)), Err(Skip::LowDisk));
    assert_eq!(decide(&g(MIN_FREE_BYTES)), Ok(()));
}

#[test]
fn youtubes_bot_check_is_read_from_the_failure() {
    for bot in [
        "the resolve: ERROR: [youtube] x: Sign in to confirm you're not a bot",
        "the download: yt-dlp exited with 1: ERROR: HTTP Error 429: Too Many Requests",
        "the resolve: This content isn't available, try again later.",
    ] {
        assert!(bot_check(bot), "{bot}");
    }
    for other in [
        "the new video: no picture",
        "the download: Video unavailable",
    ] {
        assert!(!bot_check(other), "{other}");
    }
}

#[test]
fn the_waits_are_ten_minutes_six_hours_and_six_hours() {
    assert_eq!(
        (BUSY_RETRY_MS, FAILED_RETRY_MS, BOT_PAUSE_MS),
        (600_000, 21_600_000, 21_600_000)
    );
    assert_eq!(SPACING_MS, 120_000);
}

/// The loop writes and the status route reads ONE state per process.
#[test]
fn the_worker_state_is_one_per_process() {
    assert!(std::sync::Arc::ptr_eq(&global(), &global()));
}

// ---- next_song / download_due / counts ---------------------------------------

/// Playlists 1 (active), 2 (inactive) and 3 (the test item's).
async fn catalog() -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, is_active, kind) VALUES \
         (1, 'a', 'u1', 1, 'youtube'), (2, 'b', 'u2', 0, 'youtube'), (3, 't', 'u3', 1, 'test')",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool
}

/// A row's V35 cap, state and time.
type CheckCols = (Option<i64>, Option<&'static str>, Option<i64>);

/// A downloaded row with its V35 check.
async fn song(pool: &SqlitePool, id: i64, playlist: i64, yt: &str, check: CheckCols) {
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, file_path, audio_file_path, \
         video_upgrade_cap, video_upgrade_state, video_upgrade_at) \
         VALUES (?, ?, ?, 1, '/c/v.mp4', '/c/a.flac', ?, ?, ?)",
    )
    .bind(id)
    .bind(playlist)
    .bind(yt)
    .bind(check.0)
    .bind(check.1)
    .bind(check.2)
    .execute(pool)
    .await
    .unwrap();
}

const NEVER: CheckCols = (None, None, None);

#[tokio::test]
async fn a_song_never_checked_or_checked_under_the_cap_is_picked() {
    let pool = catalog().await;
    song(
        &pool,
        1,
        1,
        "aaaaaaaaaaa",
        (Some(2160), Some("upgraded"), Some(T)),
    )
    .await;
    assert_eq!(next_song(&pool, 2160, T).await.unwrap(), None);
    song(
        &pool,
        2,
        1,
        "bbbbbbbbbbb",
        (Some(1440), Some("no_better"), Some(T)),
    )
    .await;
    assert_eq!(
        next_song(&pool, 2160, T).await.unwrap().as_deref(),
        Some("bbbbbbbbbbb")
    );
    assert_eq!(next_song(&pool, 1440, T).await.unwrap(), None);
    song(&pool, 3, 1, "ccccccccccc", NEVER).await;
    assert_eq!(
        next_song(&pool, 1440, T).await.unwrap().as_deref(),
        Some("ccccccccccc")
    );
}

#[tokio::test]
async fn a_busy_song_waits_ten_minutes_and_a_failed_one_six_hours() {
    let pool = catalog().await;
    song(
        &pool,
        1,
        1,
        "aaaaaaaaaaa",
        (None, Some("busy"), Some(T - 599_999)),
    )
    .await;
    song(
        &pool,
        2,
        1,
        "bbbbbbbbbbb",
        (None, Some("failed: x"), Some(T - 21_599_999)),
    )
    .await;
    assert_eq!(next_song(&pool, 2160, T).await.unwrap(), None);
    assert_eq!(
        next_song(&pool, 2160, T + 1).await.unwrap().as_deref(),
        Some("aaaaaaaaaaa")
    );
    sqlx::query("UPDATE videos SET video_upgrade_at = ? WHERE id = 1")
        .bind(T)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        next_song(&pool, 2160, T + 1).await.unwrap().as_deref(),
        Some("bbbbbbbbbbb")
    );
}

/// A settled check (refused too) under a raised cap is checked again.
#[tokio::test]
async fn a_refused_song_is_checked_again_only_at_a_higher_cap() {
    let pool = catalog().await;
    song(
        &pool,
        1,
        1,
        "aaaaaaaaaaa",
        (Some(1440), Some("refused: short"), Some(T)),
    )
    .await;
    assert_eq!(next_song(&pool, 1440, T).await.unwrap(), None);
    assert_eq!(
        next_song(&pool, 2160, T).await.unwrap().as_deref(),
        Some("aaaaaaaaaaa")
    );
}

#[tokio::test]
async fn active_playlists_come_first_then_the_lowest_row() {
    let pool = catalog().await;
    song(&pool, 1, 2, "inactiveaaa", NEVER).await;
    song(&pool, 5, 1, "activebbbbb", NEVER).await;
    song(&pool, 4, 1, "activeaaaaa", NEVER).await;
    assert_eq!(
        next_song(&pool, 2160, T).await.unwrap().as_deref(),
        Some("activeaaaaa")
    );
}

#[tokio::test]
async fn the_test_item_and_a_song_not_downloaded_are_never_picked() {
    let pool = catalog().await;
    song(&pool, 1, 3, "testitemaaa", NEVER).await;
    song(&pool, 2, 1, "notyetaaaaa", NEVER).await;
    sqlx::query("UPDATE videos SET normalized = 0 WHERE id = 2")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(next_song(&pool, 2160, T).await.unwrap(), None);
}

#[tokio::test]
async fn a_download_is_due_for_a_new_song_of_an_active_playlist() {
    let pool = catalog().await;
    assert!(!download_due(&pool).await.unwrap());
    sqlx::query("INSERT INTO videos (id, playlist_id, youtube_id) VALUES (1, 2, 'inactiveaaa')")
        .execute(&pool)
        .await
        .unwrap();
    assert!(!download_due(&pool).await.unwrap(), "inactive playlist");
    sqlx::query("INSERT INTO videos (id, playlist_id, youtube_id) VALUES (2, 1, 'newsongaaaa')")
        .execute(&pool)
        .await
        .unwrap();
    assert!(download_due(&pool).await.unwrap());
}

/// One count per YouTube id (two rows of one video count once).
#[tokio::test]
async fn the_counts_are_per_video() {
    let pool = catalog().await;
    song(
        &pool,
        1,
        1,
        "aaaaaaaaaaa",
        (Some(2160), Some("upgraded"), Some(T)),
    )
    .await;
    song(
        &pool,
        2,
        2,
        "aaaaaaaaaaa",
        (Some(2160), Some("upgraded"), Some(T)),
    )
    .await;
    song(
        &pool,
        3,
        1,
        "bbbbbbbbbbb",
        (Some(2160), Some("no_better"), Some(T)),
    )
    .await;
    song(
        &pool,
        4,
        1,
        "ccccccccccc",
        (Some(2160), Some("refused: x"), Some(T)),
    )
    .await;
    song(
        &pool,
        5,
        1,
        "ddddddddddd",
        (None, Some("failed: x"), Some(T)),
    )
    .await;
    song(&pool, 6, 1, "eeeeeeeeeee", (None, Some("busy"), Some(T))).await;
    song(&pool, 7, 1, "fffffffffff", NEVER).await;
    song(
        &pool,
        8,
        1,
        "ggggggggggg",
        (Some(1440), Some("no_better"), Some(T)),
    )
    .await;
    song(&pool, 9, 3, "testitemaaa", NEVER).await;
    let at_2160 = counts(&pool, 2160).await.unwrap();
    assert_eq!(
        at_2160,
        Counts {
            pending: 4,
            upgraded: 1,
            no_better: 2,
            refused: 1,
            failed: 1,
            busy: 1,
        }
    );
    assert_eq!(counts(&pool, 1440).await.unwrap().pending, 3);
}

// ---- one tick ---------------------------------------------------------------

async fn switch_on(rig: &Rig) {
    for (key, value) in [
        ("video_upgrade_enabled", "true"),
        ("video_hw_decode", "true"),
    ] {
        crate::db::models::set_setting(&rig.pool, key, value)
            .await
            .unwrap();
    }
}

const FREE: Option<u64> = Some(MIN_FREE_BYTES);

#[tokio::test]
async fn with_the_switch_off_a_tick_does_nothing() {
    let rig = Rig::new().await;
    let state = Mutex::new(WorkerState::default());
    let steps = Fake::upgrading();
    let ticked = tick(&rig.pool, rig.dir.path(), &state, &steps, FREE, NOW).await;
    assert_eq!(ticked, Ticked::Skipped(Skip::Off));
    assert!(steps.downloads.lock().unwrap().is_empty());
    assert_eq!(read_to_string(&rig.video), "old");
}

#[tokio::test]
async fn a_tick_upgrades_the_next_song_and_the_next_one_waits_its_spacing() {
    let rig = Rig::new().await;
    switch_on(&rig).await;
    let state = Mutex::new(WorkerState::default());
    let steps = Fake::upgrading();
    let ticked = tick(&rig.pool, rig.dir.path(), &state, &steps, FREE, NOW).await;
    assert_eq!(ticked, Ticked::Ran(Outcome::Upgraded));
    assert_eq!(read_to_string(&rig.video), "new");
    {
        let s = state.lock().unwrap();
        assert_eq!(s.last_start_ms, Some(NOW));
        assert_eq!(s.paused_until_ms, None);
        assert_eq!(
            s.last,
            Some(Last {
                youtube_id: YT.to_string(),
                outcome: Outcome::Upgraded,
                error: None,
                at_ms: NOW,
            })
        );
    }
    let next = tick(
        &rig.pool,
        rig.dir.path(),
        &state,
        &steps,
        FREE,
        NOW + 60_000,
    )
    .await;
    assert_eq!(next, Ticked::Skipped(Skip::Spacing));
}

#[tokio::test]
async fn a_tick_waits_for_a_due_download_and_for_the_disk() {
    let rig = Rig::new().await;
    switch_on(&rig).await;
    let state = Mutex::new(WorkerState::default());
    let steps = Fake::upgrading();
    let low = Some(MIN_FREE_BYTES - 1);
    assert_eq!(
        tick(&rig.pool, rig.dir.path(), &state, &steps, low, NOW).await,
        Ticked::Skipped(Skip::LowDisk)
    );
    sqlx::query("INSERT INTO videos (id, playlist_id, youtube_id) VALUES (20, 1, 'newsongaaaa')")
        .execute(&rig.pool)
        .await
        .unwrap();
    assert_eq!(
        tick(&rig.pool, rig.dir.path(), &state, &steps, FREE, NOW).await,
        Ticked::Skipped(Skip::DownloadDue)
    );
    assert!(steps.downloads.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_tick_is_held_by_the_background_hold() {
    let rig = Rig::new().await;
    switch_on(&rig).await;
    let until = chrono::Utc::now().timestamp_millis() + 3_600_000;
    crate::db::models::set_setting(&rig.pool, "background_hold_until_ms", &until.to_string())
        .await
        .unwrap();
    let state = Mutex::new(WorkerState::default());
    let ticked = tick(
        &rig.pool,
        rig.dir.path(),
        &state,
        &Fake::upgrading(),
        FREE,
        NOW,
    )
    .await;
    assert_eq!(ticked, Ticked::Skipped(Skip::Held));
}

/// YouTube's bot check pauses every upgrade for 6 h; any other failure
/// pauses nothing.
#[tokio::test]
async fn a_bot_check_pauses_the_worker_for_six_hours() {
    let rig = Rig::new().await;
    switch_on(&rig).await;
    let state = Mutex::new(WorkerState::default());
    let other = Fake {
        resolved: Err("ERROR: Video unavailable".into()),
        ..Fake::upgrading()
    };
    let ticked = tick(&rig.pool, rig.dir.path(), &state, &other, FREE, NOW).await;
    assert_eq!(ticked, Ticked::Ran(Outcome::Failed));
    assert_eq!(state.lock().unwrap().paused_until_ms, None);
    let bot = Fake {
        resolved: Err("ERROR: [youtube] x: Sign in to confirm you're not a bot".into()),
        ..Fake::upgrading()
    };
    let later = NOW + 21_600_000;
    let ticked = tick(&rig.pool, rig.dir.path(), &state, &bot, FREE, later).await;
    assert_eq!(ticked, Ticked::Ran(Outcome::Failed));
    assert_eq!(
        state.lock().unwrap().paused_until_ms,
        Some(later + BOT_PAUSE_MS)
    );
    let paused = tick(
        &rig.pool,
        rig.dir.path(),
        &state,
        &bot,
        FREE,
        later + 120_000,
    )
    .await;
    assert_eq!(paused, Ticked::Skipped(Skip::Paused));
}

#[tokio::test]
async fn a_tick_with_everything_checked_has_nothing_to_do() {
    let rig = Rig::new().await;
    switch_on(&rig).await;
    sqlx::query("UPDATE videos SET video_upgrade_cap = 2160, video_upgrade_state = 'no_better'")
        .execute(&rig.pool)
        .await
        .unwrap();
    let state = Mutex::new(WorkerState::default());
    let ticked = tick(
        &rig.pool,
        rig.dir.path(),
        &state,
        &Fake::upgrading(),
        FREE,
        NOW,
    )
    .await;
    assert_eq!(ticked, Ticked::Skipped(Skip::NothingToDo));
}
