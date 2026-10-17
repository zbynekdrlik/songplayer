//! #261: the cache sweep on real files — what goes, what stays, the bound,
//! the song-file lock, and the deletes never leaving the listed dirs.

use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use sqlx::SqlitePool;

use super::{PARTS_DIR, Swept, execute, is_plain_name, list, pass, row_facts};
use crate::cache_sweep_plan::{DAY_MS, Entry, Place, Plan, Planned, RowFacts, Why};
use crate::downloader::cache::{RECORDING_COLUMNS, recorded_by_a_row};

/// The pass's "now": 40 days after the real clock, so every file written
/// now reads as 40 days old unless a test gives it its own time.
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis() + 40 * DAY_MS
}

fn write(path: &Path, bytes: usize) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, vec![b'x'; bytes]).unwrap();
}

/// A file's last write set to `at_ms` (UTC ms).
fn set_modified(path: &Path, at_ms: i64) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    let at = UNIX_EPOCH + Duration::from_millis(u64::try_from(at_ms).unwrap());
    file.set_modified(at).unwrap();
}

fn song(id: &str, rest: &str) -> String {
    format!("Song_Artist_{id}_normalized{rest}")
}

/// A pool with a row of `rowvideo001` recording `video` and `audio` in
/// `cache`.
async fn pool(cache: &Path) -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, file_path, audio_file_path) \
         VALUES (1, 'rowvideo001', ?, ?)",
    )
    .bind(
        cache
            .join(song("rowvideo001", "_video.mp4"))
            .to_string_lossy()
            .into_owned(),
    )
    .bind(
        cache
            .join(song("rowvideo001", "_audio.flac"))
            .to_string_lossy()
            .into_owned(),
    )
    .execute(&pool)
    .await
    .unwrap();
    pool
}

/// Every name in `dir` (sorted), `-` for none.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|read| {
            read.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// One pass over a cache holding every kind: a row's song and its files, a
/// video no row has (its song, its id-first files, its work dir), stale
/// names, set-aside and `.prev` files, peer parts and names the sweep does
/// not know.
#[tokio::test]
async fn a_pass_deletes_what_no_row_needs_and_keeps_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let now = now_ms();
    let kept_files = [
        song("rowvideo001", "_video.mp4"),
        song("rowvideo001", "_audio.flac"),
        song("rowvideo001", "_dub.flac"),
        song("rowvideo001", "_dub_events.jsonl"),
        "rowvideo001_lyrics.json".to_string(),
        "rowvideo001_genius_cleaned_v3.json".to_string(),
        "cli-proxy-api-config.yaml".to_string(),
        "gone0000001_notes.txt".to_string(),
        "young000001_lyrics.json".to_string(),
        song("rowvideo001", "_video.mp4.replaced"),
    ];
    let gone_files = [
        song("gone0000001", "_video.mp4"),
        song("gone0000001", "_audio.flac"),
        song("gone0000001", "_audio_vocals.flac"),
        song("gone0000001", "_dub_events.jsonl"),
        "gone0000001_lyrics.json".to_string(),
        "gone0000001_vocals16k.wav".to_string(),
        "rowvideo001_whisperx_track.json".to_string(),
        "rowvideo001_genius_cleaned_v2.json".to_string(),
        song("rowvideo001", "_audio.flac.replaced"),
        song("rowvideo001", "_dub.flac.prev"),
    ];
    for name in kept_files.iter().chain(&gone_files) {
        write(&cache.join(name), 100);
    }
    // the dub was trimmed 15 days ago; a fresh file of a video no row has
    set_modified(
        &cache.join(song("rowvideo001", "_dub.flac")),
        now - 15 * DAY_MS,
    );
    set_modified(&cache.join("young000001_lyrics.json"), now - DAY_MS);
    // a `.replaced` whose name is empty stays; one beside its name goes
    let aside = cache.join(song("rowvideo001", "_video.mp4.replaced"));
    std::fs::remove_file(cache.join(song("rowvideo001", "_video.mp4"))).unwrap();
    write(&cache.join("gone0000001_isolation/seg/0001.wav"), 1_000);
    write(&cache.join("young000001_stemsep/part.flac"), 50);
    set_modified(&cache.join("young000001_stemsep/part.flac"), now - DAY_MS);
    write(&cache.join("tools/yt-dlp.exe"), 7);
    write(&cache.join("gone0000001_isolation.txt"), 1);
    write(
        &cache
            .join(PARTS_DIR)
            .join("gone0000001_audio_0123456789abcdef.part"),
        300,
    );
    write(
        &cache
            .join(PARTS_DIR)
            .join("rowvideo001_lyrics_0123456789abcdef.part"),
        40,
    );
    set_modified(
        &cache
            .join(PARTS_DIR)
            .join("rowvideo001_lyrics_0123456789abcdef.part"),
        now - DAY_MS,
    );
    write(&cache.join(PARTS_DIR).join("notes.txt"), 1);
    let pool = pool(&cache).await;

    let swept = pass(&pool, &cache, now).await.unwrap();

    let mut left: Vec<String> = kept_files
        .iter()
        .filter(|name| **name != song("rowvideo001", "_video.mp4"))
        .cloned()
        .chain([
            "gone0000001_isolation.txt".to_string(),
            "peer".to_string(),
            "tools".to_string(),
            "young000001_stemsep".to_string(),
        ])
        .collect();
    left.sort();
    assert_eq!(names(&cache), left);
    assert!(aside.exists(), "its name is empty: it may be the only copy");
    assert_eq!(
        names(&cache.join(PARTS_DIR)),
        ["notes.txt", "rowvideo001_lyrics_0123456789abcdef.part"]
    );
    assert_eq!(names(&cache.join("tools")), ["yt-dlp.exe"]);
    assert_eq!(swept.files, 11, "10 files + 1 part");
    assert_eq!(swept.dirs, 1);
    assert_eq!(swept.bytes, 10 * 100 + 1_000 + 300);
    assert_eq!(swept.count(Why::NoRow), 7, "6 files + the isolation dir");
    assert_eq!(swept.count(Why::Stale), 2);
    assert_eq!(swept.count(Why::Replaced), 1);
    assert_eq!(swept.count(Why::DubPrev), 1);
    assert_eq!(swept.count(Why::Part), 1);
    assert_eq!((swept.failed, swept.left), (0, 0));

    let again = pass(&pool, &cache, now).await.unwrap();
    assert!(again.is_empty(), "nothing left to do: {again:?}");
}

/// The pass reads the rows, lists and deletes under `cache::SONG_FILES`:
/// while another holder has it, nothing is deleted, and a row recorded
/// meanwhile (a sync adding the video back) keeps its video's files.
#[tokio::test]
async fn a_pass_waits_for_the_song_file_lock() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let gone = cache.join("gone0000001_lyrics.json");
    let back = cache.join("back0000001_lyrics.json");
    write(&gone, 1);
    write(&back, 1);
    let pool = pool(&cache).await;
    let held = crate::downloader::cache::SONG_FILES.lock().await;
    let task = {
        let (pool, cache) = (pool.clone(), cache.clone());
        tokio::spawn(async move { pass(&pool, &cache, now_ms()).await.unwrap() })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!task.is_finished(), "the pass waits for the lock");
    assert!(gone.exists());
    sqlx::query("INSERT INTO videos (playlist_id, youtube_id) VALUES (1, 'back0000001')")
        .execute(&pool)
        .await
        .unwrap();
    drop(held);
    let swept = tokio::time::timeout(Duration::from_secs(60), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(swept.count(Why::NoRow), 1);
    assert!(!gone.exists());
    assert!(back.exists(), "its row was read under the lock");
}

/// Rows of `rowvideo001`: the database has a row, so a video no row has
/// can go.
fn some_rows() -> RowFacts {
    let mut facts = RowFacts::default();
    facts.add_row("rowvideo001", &[None]);
    facts
}

/// The listing: the cache dir's files, its jobs' work dirs (a video no row
/// has: its tree's bytes and newest write; a row's video's: not walked, no
/// time), `peer/`'s files; never another dir.
#[test]
fn the_listing_reads_files_work_dirs_and_parts_only() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path();
    write(&cache.join("a_lyrics.json"), 3);
    write(&cache.join("abcdefghijk_dub/x/y.raw"), 5);
    write(&cache.join("abcdefghijk_dub/z.json"), 6);
    set_modified(&cache.join("abcdefghijk_dub/x/y.raw"), 4_102_444_800_000);
    write(&cache.join("abcdefghijk_stemsep"), 1); // a FILE of a work dir's name
    write(&cache.join("rowvideo001_isolation/seg/0001.wav"), 7);
    write(&cache.join("tools/big.exe"), 99);
    write(&cache.join(".cli-proxy-api/token.json"), 9);
    write(&cache.join("peer/p.part"), 4);
    write(&cache.join("peer/sub/q.part"), 4);
    let mut listed = list(cache, &some_rows());
    listed.sort_by(|a, b| (a.place, &a.name).cmp(&(b.place, &b.name)));
    let shape: Vec<(Place, &str, bool, u64)> = listed
        .iter()
        .map(|e| (e.place, e.name.as_str(), e.is_dir, e.bytes))
        .collect();
    assert_eq!(
        shape,
        [
            (Place::Cache, "a_lyrics.json", false, 3),
            (Place::Cache, "abcdefghijk_dub", true, 11),
            (Place::Cache, "abcdefghijk_stemsep", false, 1),
            (Place::Cache, "rowvideo001_isolation", true, 0),
            (Place::Parts, "p.part", false, 4),
        ]
    );
    let work_dir = listed.iter().find(|e| e.is_dir).unwrap();
    assert_eq!(work_dir.modified_ms, Some(4_102_444_800_000));
    let rows_dir = listed
        .iter()
        .find(|e| e.name == "rowvideo001_isolation")
        .unwrap();
    assert_eq!(rows_dir.modified_ms, None, "not walked: never the plan's");
    let file = listed.iter().find(|e| e.name == "a_lyrics.json").unwrap();
    let meta = std::fs::metadata(cache.join("a_lyrics.json")).unwrap();
    assert_eq!(file.modified_ms, crate::data_size::modified_ms(&meta));
}

/// A symlink is never listed, so neither it nor what it points at is
/// deleted, whatever its name. Unix only (the Windows runner cannot make one
/// without privilege).
#[cfg(unix)]
#[tokio::test]
async fn a_symlink_is_never_listed_or_followed() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let outside = dir.path().join("outside");
    write(&outside.join("keep.wav"), 10);
    write(&outside.join("gone0000001_lyrics.json"), 10);
    std::fs::create_dir_all(&cache).unwrap();
    std::os::unix::fs::symlink(&outside, cache.join("gone0000001_isolation")).unwrap();
    std::os::unix::fs::symlink(
        outside.join("gone0000001_lyrics.json"),
        cache.join("gone0000001_lyrics.json"),
    )
    .unwrap();
    assert_eq!(list(&cache, &some_rows()), Vec::<Entry>::new());
    let swept = pass(&pool(&cache).await, &cache, now_ms()).await.unwrap();
    assert!(swept.is_empty());
    assert_eq!(names(&outside), ["gone0000001_lyrics.json", "keep.wav"]);
    assert_eq!(names(&cache).len(), 2);
}

/// A `peer` that is a symlink is never followed: a part behind it is never
/// listed. Unix only (the Windows runner cannot make one without
/// privilege).
#[cfg(unix)]
#[test]
fn a_peer_dir_that_is_a_symlink_is_never_followed() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let outside = dir.path().join("outside");
    write(&outside.join("x_audio_0123456789abcdef.part"), 10);
    std::fs::create_dir_all(&cache).unwrap();
    std::os::unix::fs::symlink(&outside, cache.join(PARTS_DIR)).unwrap();
    assert_eq!(list(&cache, &some_rows()), Vec::<Entry>::new());
    assert_eq!(names(&outside), ["x_audio_0123456789abcdef.part"]);
}

/// A database with no row at all (fresh, or replaced) deletes no file of a
/// video, however old: every song would read as a video no row has.
#[tokio::test]
async fn a_database_with_no_row_keeps_every_video_s_files() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let song_file = cache.join(song("gone0000001", "_audio.flac"));
    write(&song_file, 10);
    write(&cache.join("gone0000001_lyrics.json"), 10);
    write(&cache.join("gone0000001_chunks.json"), 10);
    let pool = pool(&cache).await;
    sqlx::query("DELETE FROM videos")
        .execute(&pool)
        .await
        .unwrap();
    let swept = pass(&pool, &cache, now_ms()).await.unwrap();
    assert_eq!(swept.count(Why::NoRow), 0);
    assert_eq!(swept.count(Why::Stale), 1, "a retired output still goes");
    assert!(song_file.exists());
    assert!(cache.join("gone0000001_lyrics.json").exists());
    assert!(!cache.join("gone0000001_chunks.json").exists());
}

fn planned(place: Place, name: &str, is_dir: bool) -> Planned {
    Planned {
        place,
        name: name.to_string(),
        is_dir,
        bytes: 5,
        why: Why::NoRow,
    }
}

/// `(files, dirs, bytes, failed, left)` of `execute` over `delete`.
fn counts(cache: &Path, delete: Vec<Planned>, left: usize) -> (u64, u64, u64, u64, u64) {
    let swept = execute(cache, &Plan { delete, left });
    (
        swept.files,
        swept.dirs,
        swept.bytes,
        swept.failed,
        swept.left,
    )
}

/// Each case on its own (a sum would hide a swapped verdict): a file
/// already gone is no failure; one that cannot be deleted (a non-empty dir
/// where a file was planned, on Linux and Windows alike) is one failure; a
/// name that is not one plain component is one failure and is never
/// deleted; a deleted file counts with its bytes; `left` is carried.
#[test]
fn a_delete_that_cannot_happen_is_counted_and_nothing_leaves_the_dir() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    write(&cache.join("blocked/inside"), 1);
    write(&cache.join("ok_file"), 1);
    write(&dir.path().join("outside"), 1);
    let one = |name: &str| vec![planned(Place::Cache, name, false)];
    assert_eq!(counts(&cache, one("missing"), 0), (0, 0, 0, 0, 0));
    assert_eq!(counts(&cache, one("blocked"), 0), (0, 0, 0, 1, 0));
    assert!(cache.join("blocked/inside").exists());
    assert_eq!(counts(&cache, one("../outside"), 0), (0, 0, 0, 1, 0));
    assert!(dir.path().join("outside").exists());
    assert_eq!(counts(&cache, one("ok_file"), 3), (1, 0, 5, 0, 3));
    assert!(!cache.join("ok_file").exists());
}

/// A planned part is deleted in `peer/`, never in the cache dir.
#[test]
fn a_part_is_deleted_in_the_parts_dir() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path();
    write(&cache.join("x.part"), 1);
    write(&cache.join(PARTS_DIR).join("x.part"), 1);
    let plan = Plan {
        delete: vec![planned(Place::Parts, "x.part", false)],
        left: 0,
    };
    let swept = execute(cache, &plan);
    assert_eq!(swept.files, 1);
    assert!(cache.join("x.part").exists());
    assert!(!cache.join(PARTS_DIR).join("x.part").exists());
}

#[test]
fn a_plain_name_is_one_normal_component() {
    for name in [
        "a.part",
        "Song_Artist_x_normalized_audio.flac",
        ".cli-proxy-api",
    ] {
        assert!(is_plain_name(name), "{name}");
    }
    for name in ["", ".", "..", "../x", "a/b", "/a", "a\\b", "..\\x"] {
        assert!(!is_plain_name(name), "{name}");
    }
}

/// A pass is empty only with nothing deleted, failed or left.
#[test]
fn a_pass_is_empty_only_when_it_found_nothing() {
    assert!(Swept::default().is_empty());
    let with = |f: fn(&mut Swept)| {
        let mut swept = Swept::default();
        f(&mut swept);
        swept.is_empty()
    };
    assert!(!with(|s| s.files = 1));
    assert!(!with(|s| s.dirs = 1));
    assert!(!with(|s| s.failed = 1));
    assert!(!with(|s| s.left = 1));
    let mut swept = Swept::default();
    swept.by_why.insert(Why::Stale, 4);
    assert_eq!((swept.count(Why::Stale), swept.count(Why::Part)), (4, 0));
}

/// Every column that records a song's file makes `recorded_by_a_row` say so
/// and the sweep's facts keep it.
#[tokio::test]
async fn every_recording_column_records_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().to_path_buf();
    let pool = pool(&cache).await;
    for (i, column) in RECORDING_COLUMNS.iter().enumerate() {
        let name = format!("other{i:06}_lyrics.json");
        let path: PathBuf = cache.join(&name);
        assert!(!recorded_by_a_row(&pool, &path).await.unwrap(), "{column}");
        sqlx::query(&format!(
            "INSERT INTO videos (playlist_id, youtube_id, {column}) VALUES (1, 'x{i:010}', ?)"
        ))
        .bind(path.to_string_lossy().into_owned())
        .execute(&pool)
        .await
        .unwrap();
        assert!(recorded_by_a_row(&pool, &path).await.unwrap(), "{column}");
        let facts = row_facts(&pool).await.unwrap();
        let entry = Entry {
            place: Place::Cache,
            name,
            is_dir: false,
            bytes: 1,
            modified_ms: Some(0),
        };
        let files = std::collections::HashMap::new();
        let why = crate::cache_sweep_plan::why(&entry, &files, &facts, now_ms());
        assert_eq!(why, None, "{column}: recorded, kept");
    }
}
