//! #136 tests for [`relink_derived_files`] (the stems / dub re-link pass of the
//! startup self-heal). The end-to-end cases through `self_heal_cache` live in
//! `tests/startup_migration.rs`; these pin the counts and the edge rules.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sqlx::{Row, SqlitePool};

use super::*;
use crate::db;
use crate::downloader::cache;

async fn pool() -> SqlitePool {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    pool
}

/// `{base}_{id}_normalized{suffix}` in `dir`.
fn named(dir: &Path, base: &str, id: &str, suffix: &str) -> PathBuf {
    dir.join(format!("{base}_{id}_normalized{suffix}"))
}

fn text(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn write(p: &Path, content: &str, secs_ago: u64) {
    fs::write(p, content).unwrap();
    fs::File::options()
        .write(true)
        .open(p)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(secs_ago))
        .unwrap();
}

/// A row for `id` whose audio `Song_A_{id}_normalized_audio.flac` exists, with
/// the given stem / dub states (`""` = NULL stems) and the recorded paths
/// `old-v` / `old-i` / `old-d`.
async fn song(pool: &SqlitePool, dir: &Path, id: &str, stem_status: &str, dub_status: &str) -> i64 {
    let audio = named(dir, "Song_A", id, "_audio.flac");
    fs::write(&audio, b"a").unwrap();
    sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, normalized, audio_file_path,
                             stem_status, vocals_file_path, instrumental_file_path,
                             dub_status, dub_file_path)
         VALUES (1, ?, 1, ?, NULLIF(?, ''), 'old-v', 'old-i', ?, 'old-d')
         RETURNING id",
    )
    .bind(id)
    .bind(text(&audio))
    .bind(stem_status)
    .bind(dub_status)
    .fetch_one(pool)
    .await
    .unwrap()
    .get("id")
}

/// `(stem_status, vocals_file_path, instrumental_file_path, dub_file_path)`.
async fn columns(
    pool: &SqlitePool,
    id: i64,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    sqlx::query_as(
        "SELECT stem_status, vocals_file_path, instrumental_file_path, dub_file_path
         FROM videos WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Four songs, one per outcome, in one pass: every count is exact.
#[tokio::test]
async fn one_pass_counts_each_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let pool = pool().await;
    // Stems left under an old name → re-linked.
    song(&pool, d, "aaaaaaaaaaa", "done", "none").await;
    write(
        &named(d, "Old_A", "aaaaaaaaaaa", "_gf_audio_vocals.flac"),
        "v",
        10,
    );
    write(
        &named(d, "Old_A", "aaaaaaaaaaa", "_gf_audio_instrumental.flac"),
        "i",
        10,
    );
    // Stems nowhere → reset.
    song(&pool, d, "bbbbbbbbbbb", "done", "none").await;
    // A dub left under an old name → re-linked (its stems are not done).
    song(&pool, d, "ccccccccccc", "", "ready").await;
    write(&named(d, "Old_A", "ccccccccccc", "_gf_dub.flac"), "d", 10);
    // A ready dub nowhere → counted missing.
    song(&pool, d, "ddddddddddd", "", "ready").await;

    let counts = relink_derived_files(&pool, d).await.unwrap();

    assert_eq!(
        counts,
        RelinkCounts {
            stems_relinked: 1,
            stems_reset: 1,
            dubs_relinked: 1,
            dubs_missing: 1,
        }
    );
    let audio_c = named(d, "Song_A", "ccccccccccc", "_audio.flac");
    assert_eq!(
        fs::read_to_string(crate::stems::dub_path(&audio_c)).unwrap(),
        "d"
    );
}

/// Several old names: the newest one holding BOTH stems wins; a newer name with
/// only the vocals is never used (a pair is never mixed from two names).
#[tokio::test]
async fn the_newest_old_name_holding_both_stems_wins() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let pool = pool().await;
    let id = song(&pool, d, "IYAOosrh7HY", "done", "none").await;
    for (base, secs_ago) in [("Oldest_A", 900), ("Newer_A", 500)] {
        write(
            &named(d, base, "IYAOosrh7HY", "_gf_audio_vocals.flac"),
            base,
            secs_ago,
        );
        write(
            &named(d, base, "IYAOosrh7HY", "_gf_audio_instrumental.flac"),
            base,
            secs_ago,
        );
    }
    write(
        &named(d, "Partial_A", "IYAOosrh7HY", "_audio_vocals.flac"),
        "partial",
        5,
    );

    let counts = relink_derived_files(&pool, d).await.unwrap();

    assert_eq!(counts.stems_relinked, 1);
    let audio = named(d, "Song_A", "IYAOosrh7HY", "_audio.flac");
    let (vocals, instrumental) = crate::stems::stem_paths(&audio);
    assert_eq!(fs::read_to_string(&vocals).unwrap(), "Newer_A");
    assert_eq!(fs::read_to_string(&instrumental).unwrap(), "Newer_A");
    assert!(named(d, "Oldest_A", "IYAOosrh7HY", "_gf_audio_vocals.flac").exists());
    assert_eq!(
        columns(&pool, id).await,
        (
            Some("done".into()),
            Some(text(&vocals)),
            Some(text(&instrumental)),
            Some("old-d".into()),
        ),
        "the recorded stems follow the audio; the dub (not ready) is not touched"
    );
}

/// The stems come from the old name with the NEWEST STEMS, whatever else a name
/// holds: a fresh dub under an old name must not make its older stems pair win
/// (#136 review round 1).
#[tokio::test]
async fn the_stems_come_from_the_name_with_the_newest_stems_not_the_newest_file() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let pool = pool().await;
    song(&pool, d, "IYAOosrh7HY", "done", "none").await;
    for (base, secs_ago) in [("Dubbed_A", 900), ("Fresh_A", 100)] {
        write(
            &named(d, base, "IYAOosrh7HY", "_gf_audio_vocals.flac"),
            base,
            secs_ago,
        );
        write(
            &named(d, base, "IYAOosrh7HY", "_gf_audio_instrumental.flac"),
            base,
            secs_ago,
        );
    }
    write(&named(d, "Dubbed_A", "IYAOosrh7HY", "_gf_dub.flac"), "d", 1);

    let counts = relink_derived_files(&pool, d).await.unwrap();

    assert_eq!(counts.stems_relinked, 1);
    let audio = named(d, "Song_A", "IYAOosrh7HY", "_audio.flac");
    let (vocals, instrumental) = crate::stems::stem_paths(&audio);
    assert_eq!(fs::read_to_string(&vocals).unwrap(), "Fresh_A");
    assert_eq!(fs::read_to_string(&instrumental).unwrap(), "Fresh_A");
}

/// Only half a pair anywhere (the vocals under the audio's name, the
/// instrumental under an old one) is no pair: the row goes back to pending.
#[tokio::test]
async fn half_a_pair_under_each_name_is_reset() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let pool = pool().await;
    let id = song(&pool, d, "IYAOosrh7HY", "done", "none").await;
    write(
        &named(d, "Song_A", "IYAOosrh7HY", "_audio_vocals.flac"),
        "v",
        5,
    );
    write(
        &named(d, "Old_A", "IYAOosrh7HY", "_gf_audio_instrumental.flac"),
        "i",
        5,
    );

    let counts = relink_derived_files(&pool, d).await.unwrap();

    assert_eq!((counts.stems_relinked, counts.stems_reset), (0, 1));
    assert_eq!(
        columns(&pool, id).await,
        (None, None, None, Some("old-d".into()))
    );
}

/// A row whose audio is gone is not this pass's business (nothing opens stems
/// without the audio, and a reset would send the stem worker a missing file).
#[tokio::test]
async fn a_row_whose_audio_is_missing_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let pool = pool().await;
    let id = song(&pool, d, "IYAOosrh7HY", "done", "ready").await;
    fs::remove_file(named(d, "Song_A", "IYAOosrh7HY", "_audio.flac")).unwrap();

    let counts = relink_derived_files(&pool, d).await.unwrap();

    assert_eq!(counts, RelinkCounts::default());
    assert_eq!(
        columns(&pool, id).await,
        (
            Some("done".into()),
            Some("old-v".into()),
            Some("old-i".into()),
            Some("old-d".into())
        )
    );
}

/// A move that fails (a directory blocks the vocals' new name) moves nothing
/// and changes nothing: the next start tries again.
#[tokio::test]
async fn a_failed_move_leaves_the_row_for_the_next_start() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let pool = pool().await;
    let id = song(&pool, d, "IYAOosrh7HY", "done", "none").await;
    let old_vocals = named(d, "Old_A", "IYAOosrh7HY", "_gf_audio_vocals.flac");
    let old_instrumental = named(d, "Old_A", "IYAOosrh7HY", "_gf_audio_instrumental.flac");
    write(&old_vocals, "v", 5);
    write(&old_instrumental, "i", 5);
    let blocked = named(d, "Song_A", "IYAOosrh7HY", "_audio_instrumental.flac");
    fs::create_dir(&blocked).unwrap();
    fs::write(blocked.join("x"), b"x").unwrap();

    let counts = relink_derived_files(&pool, d).await.unwrap();

    assert_eq!(counts, RelinkCounts::default());
    assert_eq!(fs::read_to_string(&old_vocals).unwrap(), "v", "moved back");
    assert_eq!(fs::read_to_string(&old_instrumental).unwrap(), "i");
    assert_eq!(
        columns(&pool, id).await,
        (
            Some("done".into()),
            Some("old-v".into()),
            Some("old-i".into()),
            Some("old-d".into())
        )
    );
}

/// The dub track moves with its transcripts, and a dub already in place only
/// has its recorded path brought to the audio's name.
#[tokio::test]
async fn a_dub_moves_with_its_transcripts_and_one_in_place_is_only_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let pool = pool().await;
    let moved = song(&pool, d, "IYAOosrh7HY", "", "ready").await;
    write(&named(d, "Old_A", "IYAOosrh7HY", "_gf_dub.flac"), "d", 5);
    write(
        &named(d, "Old_A", "IYAOosrh7HY", "_gf_dub_transcripts.json"),
        "t",
        5,
    );
    let placed = song(&pool, d, "dQw4w9WgXcQ", "", "ready").await;
    let placed_audio = named(d, "Song_A", "dQw4w9WgXcQ", "_audio.flac");
    write(&crate::stems::dub_path(&placed_audio), "p", 5);

    let counts = relink_derived_files(&pool, d).await.unwrap();

    assert_eq!((counts.dubs_relinked, counts.dubs_missing), (1, 0));
    let audio = named(d, "Song_A", "IYAOosrh7HY", "_audio.flac");
    assert_eq!(
        fs::read_to_string(crate::stems::dub_path(&audio)).unwrap(),
        "d"
    );
    assert_eq!(
        fs::read_to_string(crate::stems::dub_transcripts_path(&audio)).unwrap(),
        "t"
    );
    assert_eq!(
        columns(&pool, moved).await.3,
        Some(text(&crate::stems::dub_path(&audio)))
    );
    assert_eq!(
        columns(&pool, placed).await.3,
        Some(text(&crate::stems::dub_path(&placed_audio)))
    );
}

/// `relink_song` (what a worker runs after a finished job) repairs that one
/// song and leaves every other drifted song to its own job or the next start.
#[tokio::test]
async fn relink_song_repairs_only_its_own_song() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let pool = pool().await;
    let mine = song(&pool, d, "aaaaaaaaaaa", "done", "none").await;
    let other = song(&pool, d, "bbbbbbbbbbb", "done", "none").await;
    for id in ["aaaaaaaaaaa", "bbbbbbbbbbb"] {
        write(&named(d, "Old_A", id, "_gf_audio_vocals.flac"), "v", 5);
        write(
            &named(d, "Old_A", id, "_gf_audio_instrumental.flac"),
            "i",
            5,
        );
    }

    let mine_audio = named(d, "Song_A", "aaaaaaaaaaa", "_audio.flac");
    let counts = relink_song(&pool, d, mine, &mine_audio).await.unwrap();

    assert_eq!(
        counts,
        RelinkCounts {
            stems_relinked: 1,
            ..RelinkCounts::default()
        }
    );
    assert_eq!(
        columns(&pool, mine).await.1,
        Some(text(&crate::stems::stem_paths(&mine_audio).0))
    );
    assert!(named(d, "Old_A", "bbbbbbbbbbb", "_gf_audio_vocals.flac").exists());
    assert_eq!(columns(&pool, other).await.1, Some("old-v".into()));
}

/// #136 review round 3: a re-link waits for `cache::SONG_FILES` BEFORE it reads
/// a song's row, so it acts on the row as the lock holder left it, never on a
/// read taken before (here: the stems were reset to pending meanwhile, so
/// there is nothing to re-link). The 300 ms window only lets a read that is
/// NOT behind the lock happen; correct code cannot fail it.
#[tokio::test]
async fn a_relink_reads_the_row_only_once_it_holds_the_song_files_lock() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().to_path_buf();
    let pool = pool().await;
    let id = song(&pool, &d, "IYAOosrh7HY", "done", "none").await;
    let old_vocals = named(&d, "Old_A", "IYAOosrh7HY", "_gf_audio_vocals.flac");
    write(&old_vocals, "v", 5);
    write(
        &named(&d, "Old_A", "IYAOosrh7HY", "_gf_audio_instrumental.flac"),
        "i",
        5,
    );

    let held = cache::SONG_FILES.lock().await;
    let relink = tokio::spawn({
        let (pool, d) = (pool.clone(), d.clone());
        async move {
            let audio = named(&d, "Song_A", "IYAOosrh7HY", "_audio.flac");
            relink_song(&pool, &d, id, &audio).await.unwrap()
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!relink.is_finished(), "the re-link waits for the lock");
    assert!(old_vocals.exists(), "nothing moved while the lock is held");
    sqlx::query("UPDATE videos SET stem_status = NULL WHERE id = ?")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    drop(held);
    let counts = tokio::time::timeout(Duration::from_secs(30), relink)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(counts, RelinkCounts::default(), "the row is no longer done");
    assert!(
        old_vocals.exists(),
        "a pending row's stems are not re-linked"
    );
}

/// The startup pass reads its rows under `cache::SONG_FILES` too (review round
/// 4: only `relink_song`'s order had a test). Same shape as above.
#[tokio::test]
async fn the_startup_pass_reads_its_rows_only_once_it_holds_the_song_files_lock() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().to_path_buf();
    let pool = pool().await;
    let id = song(&pool, &d, "IYAOosrh7HY", "done", "none").await;
    let old_vocals = named(&d, "Old_A", "IYAOosrh7HY", "_gf_audio_vocals.flac");
    write(&old_vocals, "v", 5);
    write(
        &named(&d, "Old_A", "IYAOosrh7HY", "_gf_audio_instrumental.flac"),
        "i",
        5,
    );

    let held = cache::SONG_FILES.lock().await;
    let pass = tokio::spawn({
        let (pool, d) = (pool.clone(), d.clone());
        async move { relink_derived_files(&pool, &d).await.unwrap() }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!pass.is_finished(), "the pass waits for the lock");
    sqlx::query("UPDATE videos SET stem_status = NULL WHERE id = ?")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    drop(held);
    let counts = tokio::time::timeout(Duration::from_secs(30), pass)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(counts, RelinkCounts::default(), "the row is no longer done");
    assert!(old_vocals.exists());
}
