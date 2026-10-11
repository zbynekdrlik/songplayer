//! #223 S12b: which `.prev` files go, and the sweep over real files.

use std::path::{Path, PathBuf};

use super::*;

const NOW: i64 = 1_760_000_000_000;
const GIB: u64 = 1024 * 1024 * 1024;

fn prev(name: &str, bytes: u64, since_ms: i64, recorded: bool, played: bool) -> PrevFile {
    PrevFile {
        path: PathBuf::from(format!("/c/{name}_video.mp4.prev")),
        bytes,
        since_ms,
        recorded,
        played,
    }
}

fn names(plan: &[(PathBuf, &'static str)]) -> Vec<(String, &'static str)> {
    plan.iter()
        .map(|(path, why)| {
            let name = path.file_name().unwrap().to_str().unwrap();
            (name.trim_end_matches("_video.mp4.prev").to_string(), *why)
        })
        .collect()
}

#[test]
fn the_limits_are_fourteen_days_and_fifteen_gib() {
    assert_eq!(PREV_KEEP_MS, 14 * 24 * 3_600_000);
    assert_eq!(PREV_BUDGET_BYTES, 15 * GIB);
}

#[test]
fn an_orphan_a_played_song_and_an_expired_one_go() {
    let files = [
        prev("orphan", 1, NOW, false, false),
        prev("played", 1, NOW, true, true),
        prev("expired", 1, NOW - PREV_KEEP_MS, true, false),
        prev("young", 1, NOW - PREV_KEEP_MS + 1, true, false),
    ];
    assert_eq!(
        names(&plan(&files, NOW)),
        [
            ("orphan".to_string(), "orphan"),
            ("played".to_string(), "played"),
            ("expired".to_string(), "expired"),
        ]
    );
}

/// Over the budget the oldest go first, only until the rest fits; exactly
/// the budget fits.
#[test]
fn over_the_budget_the_oldest_go_first() {
    let files = [
        prev("new", 5 * GIB, NOW - 1, true, false),
        prev("old", 5 * GIB, NOW - 3, true, false),
        prev("mid", 6 * GIB, NOW - 2, true, false),
    ];
    assert_eq!(names(&plan(&files, NOW)), [("old".to_string(), "budget")]);
    let at_budget = [
        prev("a", 10 * GIB, NOW - 2, true, false),
        prev("b", 5 * GIB, NOW - 1, true, false),
    ];
    assert!(plan(&at_budget, NOW).is_empty());
    let over = [
        prev("a", 10 * GIB, NOW - 2, true, false),
        prev("b", 5 * GIB + 1, NOW - 1, true, false),
    ];
    assert_eq!(names(&plan(&over, NOW)), [("a".to_string(), "budget")]);
}

/// Still over after the oldest went: the next oldest goes too.
#[test]
fn the_budget_deletes_as_many_as_it_takes() {
    let files = [
        prev("c", 8 * GIB, NOW - 1, true, false),
        prev("a", 8 * GIB, NOW - 3, true, false),
        prev("b", 8 * GIB, NOW - 2, true, false),
    ];
    assert_eq!(
        names(&plan(&files, NOW)),
        [("a".to_string(), "budget"), ("b".to_string(), "budget")]
    );
}

/// A file deleted for its own reason does not count against the budget.
#[test]
fn the_budget_counts_only_what_is_kept() {
    let files = [
        prev("played", 20 * GIB, NOW - 5, true, true),
        prev("kept", 10 * GIB, NOW - 1, true, false),
    ];
    assert_eq!(
        names(&plan(&files, NOW)),
        [("played".to_string(), "played")]
    );
}

#[test]
fn a_prev_belongs_to_the_video_named_without_prev() {
    assert_eq!(
        video_of(Path::new("/c/S_A_PySFfTurafA_normalized_video.mp4.prev")),
        Some(PathBuf::from("/c/S_A_PySFfTurafA_normalized_video.mp4"))
    );
    assert_eq!(
        video_of(Path::new("/c/S_A_PySFfTurafA_normalized_video.mp4")),
        None
    );
    assert_eq!(
        video_of(Path::new("/c/S_A_PySFfTurafA_normalized_audio.flac.prev")),
        None
    );
}

async fn pool() -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'a', 'u1')")
        .execute(&pool)
        .await
        .unwrap();
    pool
}

/// One upgraded row whose video is `video`, upgraded at `at_ms`.
async fn upgraded(pool: &SqlitePool, id: i64, yt: &str, video: &Path, at_ms: i64) {
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, file_path, \
         video_upgrade_state, video_upgrade_at) VALUES (?, 1, ?, 1, ?, 'upgraded', ?)",
    )
    .bind(id)
    .bind(yt)
    .bind(video.to_str().unwrap())
    .bind(at_ms)
    .execute(pool)
    .await
    .unwrap();
}

/// A play of row `id` recorded at `at_s` (unix seconds).
async fn played_at(pool: &SqlitePool, id: i64, at_s: i64) {
    sqlx::query(
        "INSERT INTO play_history (playlist_id, video_id, played_at) \
         VALUES (1, ?, datetime(?, 'unixepoch'))",
    )
    .bind(id)
    .bind(at_s)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn the_sweep_reads_the_rows_and_deletes_what_the_plan_names() {
    let dir = tempfile::tempdir().unwrap();
    let pool = pool().await;
    let at = 1_760_000_000_000;
    let mut videos = Vec::new();
    for (id, yt) in [(1, "playedaaaaa"), (2, "earlyplayed"), (3, "notplayeda")] {
        let video = dir.path().join(format!("S_A_{yt}_normalized_video.mp4"));
        std::fs::write(&video, "new").unwrap();
        std::fs::write(prev_path(&video), "old").unwrap();
        upgraded(&pool, id, yt, &video, at).await;
        videos.push(video);
    }
    played_at(&pool, 1, at / 1000).await; // played since the upgrade
    played_at(&pool, 2, at / 1000 - 1).await; // before it
    let orphan = dir.path().join("S_A_renamedaaa_normalized_video.mp4.prev");
    std::fs::write(&orphan, "old").unwrap();
    std::fs::write(dir.path().join("unrelated.prev"), "x").unwrap();

    let files = gather(&pool, dir.path()).await;
    assert_eq!(files.len(), 4, "{files:?}");
    let of = |video: &Path| files.iter().find(|f| f.path == prev_path(video)).unwrap();
    assert!(of(&videos[0]).played && of(&videos[0]).recorded);
    assert!(!of(&videos[1]).played);
    assert_eq!(of(&videos[2]).since_ms, at);
    assert_eq!(of(&videos[2]).bytes, 3);

    assert_eq!(sweep(&pool, dir.path(), at + 1).await, 2);
    assert!(!prev_path(&videos[0]).exists(), "played");
    assert!(
        prev_path(&videos[1]).exists(),
        "played only before its upgrade"
    );
    assert!(prev_path(&videos[2]).exists());
    assert!(!orphan.exists(), "no row names its video");
    assert!(dir.path().join("unrelated.prev").exists());
    assert_eq!(
        sweep(&pool, dir.path(), at + PREV_KEEP_MS).await,
        2,
        "expired"
    );
    assert!(!prev_path(&videos[2]).exists());
}

#[test]
fn a_prev_is_found_only_when_it_exists() {
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("a_video.mp4");
    assert_eq!(prev_of(&video), None);
    std::fs::write(prev_path(&video), "old").unwrap();
    assert_eq!(prev_of(&video), Some(prev_path(&video)));
}
