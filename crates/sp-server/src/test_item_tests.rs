//! #228: the test item's import — the pinned names, the ffmpeg arguments
//! (stream copy, FLAC with no filter / resampling / gain), the sha256 gate,
//! and the import on a real store with a transcoder that writes the files.
//! Wired via `#[cfg(test)] #[path = "test_item_tests.rs"] mod tests;` in
//! `test_item.rs`.

use std::ffi::OsString;
use std::path::Path;
use std::sync::Mutex;

use sqlx::{Row, SqlitePool};

use super::*;
use crate::peer::hash::sha256_hex;

/// What the fake clip holds (its sha256 is the import's expected one).
const CLIP: &[u8] = b"a stand-in for the measurement clip";

/// A transcoder that records every run and writes its output file (the last
/// argument): `video` for an MP4 run, `audio` for a FLAC run; or fails.
#[derive(Default)]
struct Fake {
    runs: Mutex<Vec<Vec<OsString>>>,
    fail: bool,
}

impl Fake {
    fn runs(&self) -> Vec<Vec<String>> {
        let runs = self.runs.lock().unwrap();
        let text = |args: &Vec<OsString>| -> Vec<String> {
            args.iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        runs.iter().map(text).collect()
    }
}

impl Transcoder for Fake {
    fn run(&self, args: Vec<OsString>) -> TranscodeRun<'_> {
        Box::pin(async move {
            let out = args.last().cloned().expect("an output");
            let flac = args.iter().any(|a| a == "flac");
            self.runs.lock().unwrap().push(args);
            if self.fail {
                return Err("ffmpeg failed (exit 1): no such stream".to_string());
            }
            let body: &[u8] = if flac { b"audio" } else { b"video" };
            std::fs::write(&out, body).map_err(|e| e.to_string())
        })
    }
}

async fn store() -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    pool
}

/// A cache dir and the fake clip in it.
fn rig() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let clip = dir.path().join("clip.mp4");
    std::fs::write(&clip, CLIP).unwrap();
    (dir, clip)
}

fn strings(args: Vec<OsString>) -> Vec<String> {
    args.into_iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn the_test_item_s_names_are_the_cache_layout_s() {
    assert_eq!(TEST_YOUTUBE_ID.len(), 11, "the cache's id rule");
    let (video, audio) = final_paths(Path::new("cache"));
    assert_eq!(
        video.file_name().unwrap(),
        "Meracie video v1_camera-box_measure-v01_normalized_video.mp4"
    );
    assert_eq!(
        audio.file_name().unwrap(),
        "Meracie video v1_camera-box_measure-v01_normalized_audio.flac"
    );
    assert!(video.starts_with("cache") && audio.starts_with("cache"));
    let (video_tmp, audio_tmp) = temp_paths(Path::new("cache"));
    assert_ne!(video_tmp, video);
    assert_ne!(audio_tmp, audio);
    assert_ne!(video_tmp, audio_tmp);
    assert!(video_tmp.starts_with("cache") && audio_tmp.starts_with("cache"));
}

#[test]
fn the_video_is_stream_copied() {
    let args = strings(video_args(Path::new("clip.mp4"), Path::new("v.tmp")));
    assert_eq!(
        args,
        [
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            "clip.mp4",
            "-map",
            "0:v:0",
            "-c:v",
            "copy",
            "-an",
            "-sn",
            "-dn",
            "-f",
            "mp4",
            "-y",
            "v.tmp",
        ]
    );
}

/// No loudnorm, no filter at all, no resampling, no channel change, no
/// volume: the −30 dBFS bed and the markers reach the program as they are.
#[test]
fn the_audio_is_decoded_to_flac_with_no_filter_and_no_gain() {
    let args = strings(audio_args(Path::new("clip.mp4"), Path::new("a.tmp")));
    assert_eq!(
        args,
        [
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            "clip.mp4",
            "-map",
            "0:a:0",
            "-vn",
            "-c:a",
            "flac",
            "-sample_fmt",
            "s32",
            "-f",
            "flac",
            "-y",
            "a.tmp",
        ]
    );
    for forbidden in ["-af", "-filter:a", "-filter_complex", "-ar", "-ac", "-vol"] {
        assert!(!args.iter().any(|a| a == forbidden), "{forbidden}");
    }
    assert!(
        !args
            .iter()
            .any(|a| a.contains("loudnorm") || a.contains("volume"))
    );
}

#[test]
fn the_clip_s_sha256_is_written_as_lowercase_hex() {
    assert_eq!(hex(&[0x0a, 0xff, 0x00]), "0aff00");
    let sha = clip_sha256();
    assert_eq!(sha.len(), 64);
    assert!(
        sha.starts_with("a0118ad7") && sha.ends_with("f770a748"),
        "{sha}"
    );
}

#[test]
fn the_queue_fragment_names_the_test_kind() {
    assert!(NOT_TEST_ITEM.contains(&format!("kind = '{TEST_KIND}'")));
    assert!(NOT_TEST_ITEM.starts_with("playlist_id NOT IN"));
}

#[tokio::test]
async fn the_playlist_is_made_once_as_a_looping_test_playlist() {
    let pool = store().await;
    let id = ensure_playlist(&pool).await.unwrap();
    assert_eq!(ensure_playlist(&pool).await.unwrap(), id, "made once");
    let row = sqlx::query(
        "SELECT name, youtube_url, ndi_output_name, playback_mode, is_active, kind \
         FROM playlists",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(row.len(), 1);
    let row = &row[0];
    assert_eq!(row.get::<String, _>("name"), TEST_PLAYLIST_NAME);
    assert_eq!(row.get::<String, _>("youtube_url"), "");
    assert_eq!(row.get::<String, _>("ndi_output_name"), "SP-test");
    assert_eq!(row.get::<String, _>("playback_mode"), "loop");
    assert_eq!(row.get::<i64, _>("is_active"), 1);
    assert_eq!(row.get::<String, _>("kind"), "test");
}

#[tokio::test]
async fn the_import_splits_the_clip_into_the_cache_and_records_the_row() {
    let pool = store().await;
    let (cache, clip) = rig();
    let fake = Fake::default();
    assert_eq!(
        find(&pool).await.unwrap(),
        None,
        "nothing before the import"
    );

    let (outcome, item) = import(&pool, cache.path(), &clip, &sha256_hex(CLIP), &fake)
        .await
        .unwrap();
    assert_eq!(outcome, ImportOutcome::Imported);
    let (video, audio) = final_paths(cache.path());
    let (video_tmp, audio_tmp) = temp_paths(cache.path());
    assert_eq!(
        fake.runs(),
        [
            strings(video_args(&clip, &video_tmp)),
            strings(audio_args(&clip, &audio_tmp)),
        ]
    );
    assert_eq!(std::fs::read(&video).unwrap(), b"video");
    assert_eq!(std::fs::read(&audio).unwrap(), b"audio");
    assert!(!video_tmp.exists() && !audio_tmp.exists(), "no temp left");

    let row = sqlx::query(
        "SELECT id, playlist_id, youtube_id, title, song, artist, metadata_source, \
         gemini_failed, duration_ms, file_path, audio_file_path, normalized FROM videos",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("youtube_id"), "measure-v01");
    assert_eq!(row.get::<String, _>("title"), TEST_TITLE);
    assert_eq!(row.get::<String, _>("song"), TEST_SONG);
    assert_eq!(row.get::<String, _>("artist"), TEST_ARTIST);
    assert_eq!(row.get::<String, _>("metadata_source"), "manual");
    assert_eq!(row.get::<i64, _>("gemini_failed"), 0);
    assert_eq!(row.get::<i64, _>("duration_ms"), 128_000);
    assert_eq!(row.get::<String, _>("file_path"), video.to_string_lossy());
    assert_eq!(
        row.get::<String, _>("audio_file_path"),
        audio.to_string_lossy()
    );
    assert_eq!(row.get::<i64, _>("normalized"), 1);

    assert_eq!(
        item,
        TestItem {
            playlist_id: row.get("playlist_id"),
            video_id: row.get("id"),
            youtube_id: "measure-v01".to_string(),
            ndi_output_name: "SP-test".to_string(),
            scene: "sp-test".to_string(),
            duration_ms: Some(128_000),
        }
    );
    assert_eq!(find(&pool).await.unwrap(), Some(item));
}

/// A second import of a test item whose files are in place writes nothing.
#[tokio::test]
async fn an_import_of_an_imported_item_writes_nothing() {
    let pool = store().await;
    let (cache, clip) = rig();
    let first = Fake::default();
    let (_, item) = import(&pool, cache.path(), &clip, &sha256_hex(CLIP), &first)
        .await
        .unwrap();
    let again = Fake::default();
    let (outcome, same) = import(&pool, cache.path(), &clip, &sha256_hex(CLIP), &again)
        .await
        .unwrap();
    assert_eq!(outcome, ImportOutcome::Already);
    assert_eq!(same, item);
    assert!(again.runs().is_empty(), "no ffmpeg run");
}

/// A file the row records but the disk lost is made again, under the same
/// row.
#[tokio::test]
async fn a_lost_file_is_imported_again() {
    let pool = store().await;
    let (cache, clip) = rig();
    let (_, item) = import(
        &pool,
        cache.path(),
        &clip,
        &sha256_hex(CLIP),
        &Fake::default(),
    )
    .await
    .unwrap();
    for lost in [0, 1] {
        let finals = final_paths(cache.path());
        let file = if lost == 0 { &finals.0 } else { &finals.1 };
        std::fs::remove_file(file).unwrap();
        let fake = Fake::default();
        let (outcome, again) = import(&pool, cache.path(), &clip, &sha256_hex(CLIP), &fake)
            .await
            .unwrap();
        assert_eq!(outcome, ImportOutcome::Imported, "file {lost} lost");
        assert_eq!(again, item, "the same row");
        assert_eq!(fake.runs().len(), 2);
        assert!(file.is_file());
    }
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM videos")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 1);
}

/// Any file but the pinned clip is refused before anything is written.
#[tokio::test]
async fn a_file_that_is_not_the_clip_is_refused_and_nothing_is_written() {
    let pool = store().await;
    let (cache, clip) = rig();
    let fake = Fake::default();
    let other = sha256_hex(b"the clip camera-box delivered");
    let refused = import(&pool, cache.path(), &clip, &other, &fake).await;
    match refused {
        Err(ImportError::WrongFile(found)) => assert_eq!(found, sha256_hex(CLIP)),
        other => panic!("expected WrongFile, got {other:?}"),
    }
    assert!(fake.runs().is_empty());
    let playlists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM playlists")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(playlists, 0, "no playlist made");
    assert!(!final_paths(cache.path()).0.exists());
}

#[tokio::test]
async fn a_missing_clip_is_a_read_error() {
    let pool = store().await;
    let (cache, _clip) = rig();
    let missing = cache.path().join("no-such-clip.mp4");
    let result = import(&pool, cache.path(), &missing, "", &Fake::default()).await;
    assert!(matches!(result, Err(ImportError::Read(_))), "{result:?}");
}

/// A failed ffmpeg run records nothing and leaves no temp file.
#[tokio::test]
async fn a_failed_transcode_records_nothing_and_leaves_no_temp() {
    let pool = store().await;
    let (cache, clip) = rig();
    let (video_tmp, _) = temp_paths(cache.path());
    std::fs::write(&video_tmp, b"half a run").unwrap();
    let fake = Fake {
        fail: true,
        ..Fake::default()
    };
    let result = import(&pool, cache.path(), &clip, &sha256_hex(CLIP), &fake).await;
    assert!(
        matches!(result, Err(ImportError::Transcode(_))),
        "{result:?}"
    );
    assert_eq!(
        fake.runs().len(),
        1,
        "the audio is not run after a failed video"
    );
    assert!(!video_tmp.exists(), "the temp is removed");
    let videos: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM videos")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(videos, 0);
    assert_eq!(find(&pool).await.unwrap(), None);
}

#[test]
fn removing_a_temp_that_is_not_there_is_fine_and_a_real_failure_is_not() {
    let dir = tempfile::tempdir().unwrap();
    assert!(remove_temp(&dir.path().join("absent")).is_ok());
    let file = dir.path().join("present");
    std::fs::write(&file, b"x").unwrap();
    assert!(remove_temp(&file).is_ok());
    assert!(!file.exists());
    // A directory cannot be removed as a file.
    assert!(remove_temp(dir.path()).is_err());
}
