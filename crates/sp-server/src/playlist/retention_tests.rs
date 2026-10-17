//! #260 (ROZHODNUTÉ 13.10.2026): a song that left its YouTube playlist is
//! deleted with its files a year after its mark, never a file another row
//! still records, never a song brought back.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use sqlx::SqlitePool;

use super::{FilePlan, PassReport, RETENTION_BATCH, candidates, cutoff_ms, pass, plan_files};
use crate::downloader::cache::{SongFiles, derived_files};
use crate::playlist::PlaylistEntry;

const NOW: i64 = 1_800_000_000_000;
const DAY: i64 = 86_400_000;
const HOUR: i64 = 3_600_000;

/// A row's song on disk: its video, its audio and the files named after it.
struct Song {
    id: i64,
    video: PathBuf,
    audio: PathBuf,
    derived: [PathBuf; 4],
}

impl Song {
    fn all(&self) -> Vec<PathBuf> {
        let mut out = vec![self.video.clone(), self.audio.clone()];
        out.extend(self.derived.iter().cloned());
        out
    }
}

/// Playlists 1 and 2 (youtube) on a fresh database.
async fn pool() -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'fast', 'u1'), (2, 'slow', 'u2')",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool
}

/// A downloaded row of `youtube_id` in `playlist`, marked at `removed_at`,
/// its files named in `dir` and written there when `on_disk`.
async fn song(
    pool: &SqlitePool,
    dir: &Path,
    playlist: i64,
    youtube_id: &str,
    removed_at: Option<i64>,
    on_disk: bool,
) -> Song {
    let video = dir.join(format!("Song_Artist_{youtube_id}_normalized_video.mp4"));
    let audio = dir.join(format!("Song_Artist_{youtube_id}_normalized_audio.flac"));
    let derived = derived_files(&audio);
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, title, normalized, file_path, \
         audio_file_path, removed_at) VALUES (?, ?, ?, 1, ?, ?, ?) RETURNING id",
    )
    .bind(playlist)
    .bind(youtube_id)
    .bind(format!("Title {youtube_id}"))
    .bind(video.to_string_lossy().into_owned())
    .bind(audio.to_string_lossy().into_owned())
    .bind(removed_at)
    .fetch_one(pool)
    .await
    .unwrap();
    let song = Song {
        id,
        video,
        audio,
        derived,
    };
    if on_disk {
        for path in song.all() {
            std::fs::write(&path, b"x").unwrap();
        }
    }
    song
}

async fn row_exists(pool: &SqlitePool, id: i64) -> bool {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM videos WHERE id = ?)")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn count(pool: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

fn on_disk(paths: &[PathBuf]) -> Vec<bool> {
    paths.iter().map(|p| p.exists()).collect()
}

/// The owner's year: 365 days back from now.
#[test]
fn the_year_is_counted_back_from_now() {
    assert_eq!(cutoff_ms(NOW), NOW - 365 * DAY);
}

fn set_of(video: &str, audio: &str) -> SongFiles {
    SongFiles {
        video: Some(PathBuf::from(video)),
        audio: Some(PathBuf::from(audio)),
    }
}

#[test]
fn a_song_no_other_row_records_loses_every_file_video_first() {
    let files = set_of("/c/s_video.mp4", "/c/s_audio.flac");
    let mut all = vec![
        PathBuf::from("/c/s_video.mp4"),
        PathBuf::from("/c/s_audio.flac"),
    ];
    all.extend(derived_files(Path::new("/c/s_audio.flac")));
    assert_eq!(candidates(&files), all);
    assert_eq!(
        plan_files(&files, &HashSet::new()),
        FilePlan {
            remove: all,
            keep: vec![],
        }
    );
}

/// Another row records the audio but has its own video: the audio and
/// everything named after it stay, the video goes.
#[test]
fn a_shared_audio_keeps_its_stems_and_dub_but_not_another_video() {
    let files = set_of("/c/s_video.mp4", "/c/s_audio.flac");
    let elsewhere = HashSet::from([PathBuf::from("/c/s_audio.flac")]);
    let mut keep = vec![PathBuf::from("/c/s_audio.flac")];
    keep.extend(derived_files(Path::new("/c/s_audio.flac")));
    assert_eq!(
        plan_files(&files, &elsewhere),
        FilePlan {
            remove: vec![PathBuf::from("/c/s_video.mp4")],
            keep,
        }
    );
}

#[test]
fn a_shared_video_and_audio_keep_the_whole_set() {
    let files = set_of("/c/s_video.mp4", "/c/s_audio.flac");
    let elsewhere = HashSet::from([
        PathBuf::from("/c/s_video.mp4"),
        PathBuf::from("/c/s_audio.flac"),
    ]);
    let plan = plan_files(&files, &elsewhere);
    assert_eq!(plan.remove, Vec::<PathBuf>::new());
    assert_eq!(plan.keep, candidates(&files));
}

/// A stem another row records by name (its own column) stays even when
/// the audio goes.
#[test]
fn a_derived_file_another_row_records_stays() {
    let files = set_of("/c/s_video.mp4", "/c/s_audio.flac");
    let derived = derived_files(Path::new("/c/s_audio.flac"));
    let elsewhere = HashSet::from([derived[0].clone()]);
    let plan = plan_files(&files, &elsewhere);
    assert_eq!(plan.keep, vec![derived[0].clone()]);
    assert_eq!(plan.remove.len(), 5);
    assert!(!plan.remove.contains(&derived[0]));
}

#[test]
fn a_row_with_no_files_has_nothing_to_delete() {
    let files = SongFiles {
        video: None,
        audio: None,
    };
    assert_eq!(candidates(&files), Vec::<PathBuf>::new());
    assert_eq!(plan_files(&files, &HashSet::new()), FilePlan::default());
}

/// The edge: a mark exactly 365 days old goes, 1 ms younger or 364 days
/// 23 h stays (row and files).
#[tokio::test]
async fn a_song_goes_exactly_a_year_after_it_left() {
    let pool = pool().await;
    let dir = tempfile::tempdir().unwrap();
    let year = song(
        &pool,
        dir.path(),
        1,
        "year-v00001",
        Some(NOW - 365 * DAY),
        true,
    )
    .await;
    let almost = song(
        &pool,
        dir.path(),
        1,
        "almost-v001",
        Some(NOW - 365 * DAY + 1),
        true,
    )
    .await;
    let day_less = song(
        &pool,
        dir.path(),
        1,
        "dayless-v01",
        Some(NOW - 364 * DAY - 23 * HOUR),
        true,
    )
    .await;
    let report = pass(&pool, NOW).await.unwrap();
    assert_eq!(report.failed, 0);
    assert_eq!(report.deleted.len(), 1);
    let gone = &report.deleted[0];
    assert_eq!(gone.youtube_id, "year-v00001");
    assert_eq!(gone.title.as_deref(), Some("Title year-v00001"));
    assert_eq!(gone.removed, year.all(), "every file, the video first");
    assert_eq!(gone.kept, Vec::<PathBuf>::new());
    assert!(!row_exists(&pool, year.id).await);
    assert_eq!(on_disk(&year.all()), vec![false; 6]);
    for kept in [&almost, &day_less] {
        assert!(row_exists(&pool, kept.id).await);
        assert_eq!(on_disk(&kept.all()), vec![true; 6]);
    }
}

/// The same video in another playlist records the same files: the marked
/// row goes, every file stays (the stems and dub with the shared audio),
/// and so do the other row's history and the exchange's records of the
/// video.
#[tokio::test]
async fn a_file_another_row_records_is_never_deleted() {
    let pool = pool().await;
    let dir = tempfile::tempdir().unwrap();
    let marked = song(
        &pool,
        dir.path(),
        1,
        "shared-v001",
        Some(NOW - 400 * DAY),
        true,
    )
    .await;
    let other: i64 = sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, normalized, file_path, audio_file_path) \
         SELECT 2, youtube_id, 1, file_path, audio_file_path FROM videos WHERE id = ? \
         RETURNING id",
    )
    .bind(marked.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    for id in [marked.id, other] {
        sqlx::query(
            "INSERT INTO play_history (playlist_id, video_id) \
             SELECT playlist_id, id FROM videos WHERE id = ?",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO peer_waits (youtube_id, job, since_ms) VALUES ('shared-v001', 'lyrics', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let report = pass(&pool, NOW).await.unwrap();
    assert_eq!(report.deleted.len(), 1);
    assert_eq!(report.deleted[0].removed, Vec::<PathBuf>::new());
    assert_eq!(report.deleted[0].kept, marked.all());
    assert!(!row_exists(&pool, marked.id).await);
    assert!(row_exists(&pool, other).await);
    assert_eq!(on_disk(&marked.all()), vec![true; 6]);
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM play_history").await,
        1,
        "the other row's history stays"
    );
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM peer_waits").await,
        1,
        "a row of the video is left"
    );
}

/// The last row of a video goes with its own data: its play history, the
/// exchange's records of the video, the sha256 cache entries of its files.
/// Another video's records stay.
#[tokio::test]
async fn the_last_row_of_a_video_goes_with_its_own_data() {
    let pool = pool().await;
    let dir = tempfile::tempdir().unwrap();
    let marked = song(
        &pool,
        dir.path(),
        1,
        "own-data-01",
        Some(NOW - 400 * DAY),
        true,
    )
    .await;
    let kept = song(&pool, dir.path(), 1, "other-vid01", None, true).await;
    for (yt, s) in [("own-data-01", &marked), ("other-vid01", &kept)] {
        sqlx::query("INSERT INTO play_history (playlist_id, video_id) VALUES (1, ?)")
            .bind(s.id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO peer_waits (youtube_id, job, since_ms) VALUES (?, 'stems', 1)")
            .bind(yt)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO peer_fetches (youtube_id, kind, node, version, sha256, fetched_at_ms) \
             VALUES (?, 'audio', 'snv', 1, 'sha', 1)",
        )
        .bind(yt)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO peer_standins (youtube_id, job, peer, made_at_ms, next_check_ms) \
             VALUES (?, 'lyrics', 'snv', 1, 1)",
        )
        .bind(yt)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO peer_hashes (path, size, mtime_ms, sha256, hashed_at_ms) \
             VALUES (?, 1, 1, 'sha', 1)",
        )
        .bind(s.audio.to_string_lossy().into_owned())
        .execute(&pool)
        .await
        .unwrap();
    }
    let report = pass(&pool, NOW).await.unwrap();
    assert_eq!(report.deleted.len(), 1);
    for table in [
        "play_history",
        "peer_waits",
        "peer_fetches",
        "peer_standins",
        "peer_hashes",
    ] {
        assert_eq!(
            count(&pool, &format!("SELECT COUNT(*) FROM {table}")).await,
            1,
            "{table}: only the other video's record is left"
        );
    }
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM peer_waits WHERE youtube_id = 'other-vid01'"
        )
        .await,
        1
    );
    let hashed: String = sqlx::query_scalar("SELECT path FROM peer_hashes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(hashed, kept.audio.to_string_lossy());
}

/// A song put back in the YouTube playlist before the year is in it again
/// (the sync cleared its mark): never deleted.
#[tokio::test]
async fn a_song_brought_back_is_never_deleted() {
    let pool = pool().await;
    let dir = tempfile::tempdir().unwrap();
    let back = song(
        &pool,
        dir.path(),
        1,
        "back-v00001",
        Some(NOW - 400 * DAY),
        true,
    )
    .await;
    let entry = PlaylistEntry {
        id: "back-v00001".to_string(),
        title: None,
        duration_ms: None,
    };
    crate::playlist::membership::record_fetch(&pool, 1, &[entry], NOW)
        .await
        .unwrap();
    assert_eq!(pass(&pool, NOW).await.unwrap(), PassReport::default());
    assert!(row_exists(&pool, back.id).await);
    assert_eq!(on_disk(&back.all()), vec![true; 6]);
}

/// A pass deletes at most `RETENTION_BATCH` rows; the next pass the rest.
#[tokio::test]
async fn a_pass_deletes_at_most_a_batch() {
    let pool = pool().await;
    let dir = tempfile::tempdir().unwrap();
    for i in 0..(RETENTION_BATCH + 2) {
        song(
            &pool,
            dir.path(),
            1,
            &format!("batch-{i:05}"),
            Some(NOW - 400 * DAY),
            false,
        )
        .await;
    }
    assert_eq!(pass(&pool, NOW).await.unwrap().deleted.len(), 50);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM videos").await, 2);
    assert_eq!(pass(&pool, NOW).await.unwrap().deleted.len(), 2);
    assert_eq!(pass(&pool, NOW).await.unwrap(), PassReport::default());
}

/// Nothing due: a pass touches no row and no file.
#[tokio::test]
async fn a_pass_with_nothing_due_touches_nothing() {
    let pool = pool().await;
    let dir = tempfile::tempdir().unwrap();
    let listed = song(&pool, dir.path(), 1, "listed-v001", None, true).await;
    let recent = song(&pool, dir.path(), 1, "recent-v001", Some(NOW - DAY), true).await;
    assert_eq!(pass(&pool, NOW).await.unwrap(), PassReport::default());
    for s in [&listed, &recent] {
        assert!(row_exists(&pool, s.id).await);
        assert_eq!(on_disk(&s.all()), vec![true; 6]);
    }
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM videos").await, 2);
}

/// A row in a custom playlist's set list is the operator's: never deleted.
#[tokio::test]
async fn a_song_in_a_set_list_is_never_deleted() {
    let pool = pool().await;
    let dir = tempfile::tempdir().unwrap();
    let in_set = song(
        &pool,
        dir.path(),
        1,
        "setlist-v01",
        Some(NOW - 400 * DAY),
        true,
    )
    .await;
    let custom: i64 = sqlx::query_scalar(
        "INSERT INTO playlists (name, youtube_url, kind) VALUES ('live', '', 'custom') \
         RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    crate::db::models::append_playlist_item(&pool, custom, in_set.id)
        .await
        .unwrap();
    assert_eq!(pass(&pool, NOW).await.unwrap(), PassReport::default());
    assert!(row_exists(&pool, in_set.id).await);
    assert_eq!(on_disk(&in_set.all()), vec![true; 6]);
}

/// A file that cannot be deleted (here a directory in its place): a WARN,
/// the row and its other files stay for the next pass (the video goes
/// first, so nothing else was touched); the rest of the batch goes on.
#[tokio::test]
async fn a_file_that_cannot_be_deleted_keeps_the_row_for_the_next_pass() {
    let pool = pool().await;
    let dir = tempfile::tempdir().unwrap();
    let stuck = song(
        &pool,
        dir.path(),
        1,
        "stuck-v0001",
        Some(NOW - 401 * DAY),
        false,
    )
    .await;
    std::fs::create_dir_all(stuck.video.join("inside")).unwrap();
    std::fs::write(&stuck.audio, b"x").unwrap();
    let fine = song(
        &pool,
        dir.path(),
        1,
        "fine-v00001",
        Some(NOW - 400 * DAY),
        true,
    )
    .await;
    let report = pass(&pool, NOW).await.unwrap();
    assert_eq!(report.failed, 1);
    assert_eq!(report.deleted.len(), 1);
    assert_eq!(report.deleted[0].youtube_id, "fine-v00001");
    assert!(row_exists(&pool, stuck.id).await);
    assert!(
        stuck.audio.exists(),
        "the audio after the stuck video stays"
    );
    assert!(!row_exists(&pool, fine.id).await);
}
