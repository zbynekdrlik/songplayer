//! #228: the test item's import — the pinned names, the ffmpeg arguments
//! (stream copy, FLAC with no filter / resampling / gain), the sha256 gate,
//! and the import on a real store with a transcoder that writes the files.
//! Wired via `#[cfg(test)] #[path = "test_item_tests.rs"] mod tests;` in
//! `test_item.rs`.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::Path;
use std::sync::Mutex;

use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};

use super::*;
use crate::peer::hash::sha256_hex;

/// What the fake clip holds (its sha256 is the import's expected one).
const CLIP: &[u8] = b"a stand-in for the measurement clip";

/// `bytes`' sha256 as the byte array a [`Clip`] pins.
fn sha_bytes(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(bytes));
    out
}

/// camera-box's clip, pinned to the fake clip's bytes.
fn fake_clip() -> Clip {
    Clip {
        sha256: sha_bytes(CLIP),
        ..CAMERA_BOX
    }
}

/// A transcoder that records every run and writes its output file (the last
/// argument): `video` for an MP4 run, `audio` for a FLAC run; or fails —
/// every run (`fail`), or a run with an argument containing `fail_on`.
#[derive(Default)]
struct Fake {
    runs: Mutex<Vec<Vec<OsString>>>,
    fail: bool,
    fail_on: Option<&'static str>,
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
            let named = |needle: &str| args.iter().any(|a| a.to_string_lossy().contains(needle));
            let fail = self.fail || self.fail_on.is_some_and(named);
            self.runs.lock().unwrap().push(args);
            if fail {
                return Err("ffmpeg failed (exit 1): no such stream".to_string());
            }
            let body: &[u8] = if flac { b"audio" } else { b"video" };
            std::fs::write(out, body).map_err(|e| e.to_string())
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
    let (video, audio) = final_paths(Path::new("cache"), &CAMERA_BOX);
    assert_eq!(
        video.file_name().unwrap(),
        "Meracie video v1_camera-box_measure-v01_normalized_video.mp4"
    );
    assert_eq!(
        audio.file_name().unwrap(),
        "Meracie video v1_camera-box_measure-v01_normalized_audio.flac"
    );
    assert!(video.starts_with("cache") && audio.starts_with("cache"));
    let (video_tmp, audio_tmp) = temp_paths(Path::new("cache"), &CAMERA_BOX);
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

    let (outcome, imported_as, item) = import(&pool, cache.path(), &clip, &[fake_clip()], &fake)
        .await
        .unwrap();
    assert_eq!(outcome, ImportOutcome::Imported);
    assert_eq!(imported_as, fake_clip());
    let (video, audio) = final_paths(cache.path(), &CAMERA_BOX);
    let (video_tmp, audio_tmp) = temp_paths(cache.path(), &CAMERA_BOX);
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
    let (_, _, item) = import(&pool, cache.path(), &clip, &[fake_clip()], &first)
        .await
        .unwrap();
    let again = Fake::default();
    let (outcome, _, same) = import(&pool, cache.path(), &clip, &[fake_clip()], &again)
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
    let (_, _, item) = import(&pool, cache.path(), &clip, &[fake_clip()], &Fake::default())
        .await
        .unwrap();
    for lost in [0, 1] {
        let finals = final_paths(cache.path(), &CAMERA_BOX);
        let file = if lost == 0 { &finals.0 } else { &finals.1 };
        std::fs::remove_file(file).unwrap();
        let fake = Fake::default();
        let (outcome, _, again) = import(&pool, cache.path(), &clip, &[fake_clip()], &fake)
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
    let other = Clip {
        sha256: sha_bytes(b"the clip camera-box delivered"),
        ..CAMERA_BOX
    };
    let refused = import(&pool, cache.path(), &clip, &[other], &fake).await;
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
    assert!(!final_paths(cache.path(), &CAMERA_BOX).0.exists());
}

#[tokio::test]
async fn a_missing_clip_is_a_read_error() {
    let pool = store().await;
    let (cache, _clip) = rig();
    let missing = cache.path().join("no-such-clip.mp4");
    let result = import(&pool, cache.path(), &missing, &CLIPS, &Fake::default()).await;
    assert!(matches!(result, Err(ImportError::Read(_))), "{result:?}");
}

/// A failed ffmpeg run records nothing and leaves no temp file.
#[tokio::test]
async fn a_failed_transcode_records_nothing_and_leaves_no_temp() {
    let pool = store().await;
    let (cache, clip) = rig();
    let (video_tmp, _) = temp_paths(cache.path(), &CAMERA_BOX);
    std::fs::write(&video_tmp, b"half a run").unwrap();
    let fake = Fake {
        fail: true,
        ..Fake::default()
    };
    let result = import(&pool, cache.path(), &clip, &[fake_clip()], &fake).await;
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

// #272: the pinned clip LIST.

#[test]
fn the_clip_list_is_camera_box_s_clip_then_the_wall_clips() {
    let names: Vec<&str> = CLIPS.iter().map(|c| c.name).collect();
    assert_eq!(names, ["camera-box", "wall-24", "wall-25", "wall-30"]);
    assert_eq!(CLIPS[0], CAMERA_BOX, "camera-box's clip is the default");
    // camera-box's clip is #228's test item, unchanged.
    assert_eq!(
        (CAMERA_BOX.file, CAMERA_BOX.youtube_id, CAMERA_BOX.fps),
        (TEST_CLIP_FILE, "measure-v01", 30)
    );
    assert_eq!(CAMERA_BOX.sha256_hex(), clip_sha256());
    assert_eq!(
        (CAMERA_BOX.song, CAMERA_BOX.artist, CAMERA_BOX.title),
        (TEST_SONG, TEST_ARTIST, TEST_TITLE)
    );
    assert_eq!(CAMERA_BOX.duration_ms, 128_000);
    for (clip, fps) in [(WALL_24, 24), (WALL_25, 25), (WALL_30, 30)] {
        assert_eq!(clip.fps, fps);
        assert_eq!(clip.name, format!("wall-{fps}"));
        assert_eq!(clip.file, format!("wall-clip-v2-{fps}fps-60s.mp4"));
        // The row stays (`wallclip-{fps}`): the v2 import records it anew.
        assert_eq!(clip.youtube_id, format!("wallclip-{fps}"));
        // A v2 song name, so the cache names differ from v1's and the
        // import's `already` check never takes the v1 files for v2.
        assert_eq!(clip.song, format!("Wall clip v2 {fps} fps"));
        assert_eq!(
            clip.title,
            format!("SongPlayer wall clip v2, {fps} fps (60 s)")
        );
        assert_eq!(clip.artist, WALL_ARTIST);
        assert_eq!(clip.duration_ms, WALL_DURATION_MS);
    }
    assert_eq!((WALL_ARTIST, WALL_DURATION_MS), ("SongPlayer", 60_000));
    for clip in &CLIPS {
        let id = clip.youtube_id;
        assert_eq!(id.len(), 11, "the cache's id rule: {id}");
        let id_char = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
        assert!(id.chars().all(id_char), "{id}");
    }
    let unique = |values: Vec<String>| values.iter().collect::<HashSet<_>>().len();
    assert_eq!(
        unique(CLIPS.iter().map(|c| c.youtube_id.into()).collect()),
        4
    );
    assert_eq!(unique(CLIPS.iter().map(|c| c.file.into()).collect()), 4);
    assert_eq!(unique(CLIPS.iter().map(Clip::sha256_hex).collect()), 4);
}

/// The wall clips' pins are the clip v2 files `scripts/wall_clip.py` made on
/// dev1 (20.10.2026, #272: copies across the width under rulers, read under
/// any zoom of it), byte-identical on a second run.
#[test]
fn the_wall_clips_are_pinned_to_the_generated_files() {
    let pins = [
        (WALL_24, "7e7eaa4a", "a5a12df0"),
        (WALL_25, "7a9bdc6b", "4dac9d70"),
        (WALL_30, "f2134902", "49d3128d"),
    ];
    for (clip, head, tail) in pins {
        let sha = clip.sha256_hex();
        assert_eq!(sha.len(), 64);
        assert!(sha.starts_with(head) && sha.ends_with(tail), "{sha}");
    }
}

#[test]
fn a_clip_is_found_by_its_name_and_nothing_else_is() {
    for clip in &CLIPS {
        assert_eq!(clip_named(clip.name), Some(clip));
    }
    for name in ["", "wall-26", "Wall-25", "camera-box ", "measure-v01"] {
        assert_eq!(clip_named(name), None, "{name:?}");
    }
}

#[test]
fn every_clip_has_its_own_cache_names() {
    let cache = Path::new("cache");
    let (video, audio) = final_paths(cache, &WALL_25);
    assert_eq!(
        video.file_name().unwrap(),
        "Wall clip v2 25 fps_SongPlayer_wallclip-25_normalized_video.mp4"
    );
    assert_eq!(
        audio.file_name().unwrap(),
        "Wall clip v2 25 fps_SongPlayer_wallclip-25_normalized_audio.flac"
    );
    let (video_tmp, audio_tmp) = temp_paths(cache, &WALL_25);
    assert_eq!(
        video_tmp.file_name().unwrap(),
        "test-item-wallclip-25.importing-video"
    );
    assert_eq!(
        audio_tmp.file_name().unwrap(),
        "test-item-wallclip-25.importing-audio"
    );
    let mut names = HashSet::new();
    for clip in &CLIPS {
        let (video, audio) = final_paths(cache, clip);
        let (video_tmp, audio_tmp) = temp_paths(cache, clip);
        for path in [video, audio, video_tmp, audio_tmp] {
            assert!(path.starts_with(cache));
            assert!(names.insert(path), "{}", clip.name);
        }
    }
}

/// Two listed clips, each pinned to its own stand-in bytes.
const WALL_BYTES: &[u8] = b"a stand-in for the 25 fps wall clip";

fn two_clips() -> [Clip; 2] {
    let wall = Clip {
        sha256: sha_bytes(WALL_BYTES),
        ..WALL_25
    };
    [fake_clip(), wall]
}

/// A file is imported as the listed clip whose sha256 it has: that clip's
/// row (its id, title, length) and names, in the one test playlist.
#[tokio::test]
async fn an_import_takes_the_listed_clip_the_file_is() {
    let pool = store().await;
    let (cache, _) = rig();
    let clips = two_clips();
    let file = cache.path().join("any name.mp4");
    std::fs::write(&file, WALL_BYTES).unwrap();
    let fake = Fake::default();
    let (outcome, clip, item) = import(&pool, cache.path(), &file, &clips, &fake)
        .await
        .unwrap();
    assert_eq!(outcome, ImportOutcome::Imported);
    assert_eq!(clip, clips[1]);
    assert_eq!(item.youtube_id, "wallclip-25");
    assert_eq!(item.duration_ms, Some(60_000));
    assert_eq!(
        (item.ndi_output_name.as_str(), item.scene.as_str()),
        ("SP-test", "sp-test")
    );
    let (video, audio) = final_paths(cache.path(), &WALL_25);
    assert_eq!(std::fs::read(&video).unwrap(), b"video");
    assert_eq!(std::fs::read(&audio).unwrap(), b"audio");
    let row = sqlx::query("SELECT title, song, artist, metadata_source FROM videos")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("title"), WALL_25.title);
    assert_eq!(row.get::<String, _>("song"), WALL_25.song);
    assert_eq!(row.get::<String, _>("artist"), "SongPlayer");
    assert_eq!(row.get::<String, _>("metadata_source"), "manual");
    assert_eq!(
        find_clip(&pool, &WALL_25).await.unwrap(),
        Some(item.clone())
    );
    assert_eq!(
        find(&pool).await.unwrap(),
        None,
        "camera-box's is not imported"
    );
    assert_eq!(find_any(&pool).await.unwrap(), Some(item));
}

/// Every clip is a row of the ONE test playlist; `find_any` takes the first
/// listed clip that is imported.
#[tokio::test]
async fn every_clip_is_a_row_of_the_one_test_playlist() {
    let pool = store().await;
    let (cache, clip_file) = rig();
    let clips = two_clips();
    assert_eq!(find_any(&pool).await.unwrap(), None, "nothing imported");
    let wall_file = cache.path().join("wall.mp4");
    std::fs::write(&wall_file, WALL_BYTES).unwrap();
    let fake = Fake::default();
    let (_, _, wall) = import(&pool, cache.path(), &wall_file, &clips, &fake)
        .await
        .unwrap();
    assert_eq!(find_any(&pool).await.unwrap(), Some(wall.clone()));
    let (_, _, camera) = import(&pool, cache.path(), &clip_file, &clips, &fake)
        .await
        .unwrap();
    assert_eq!(camera.playlist_id, wall.playlist_id, "one playlist");
    assert_ne!(camera.video_id, wall.video_id);
    assert_eq!(
        find_any(&pool).await.unwrap(),
        Some(camera),
        "the list's order"
    );
    let playlists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM playlists")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(playlists, 1);
}

/// The list import: each clip read under its OWN file name; absent is no
/// error, a file that is not its clip and a failed ffmpeg are named, a
/// clip already in place is left as it is.
#[tokio::test]
async fn the_list_import_takes_each_clip_found_under_its_own_name() {
    use ClipOutcome::{Absent, Already, Failed, Imported, WrongFile};
    let pool = store().await;
    let (cache, _) = rig();
    let bench = tempfile::tempdir().unwrap();
    let pinned = |bytes: &[u8], clip: Clip| Clip {
        sha256: sha_bytes(bytes),
        ..clip
    };
    let a = pinned(b"clip a", CAMERA_BOX);
    let b = pinned(b"clip b", WALL_24);
    let c = pinned(b"clip c", WALL_25);
    let d = pinned(b"clip d", WALL_30);
    std::fs::write(bench.path().join(a.file), b"clip a").unwrap();
    std::fs::write(bench.path().join(c.file), b"not clip c").unwrap();
    std::fs::write(bench.path().join(d.file), b"clip d").unwrap();
    let list = [a, b, c, d];

    let fake = Fake {
        fail_on: Some(d.file),
        ..Fake::default()
    };
    let answers = import_listed(&pool, cache.path(), bench.path(), &list, &fake).await;
    let outcomes: Vec<_> = answers.iter().map(|x| (x.name, x.outcome)).collect();
    assert_eq!(
        outcomes,
        [
            ("camera-box", Imported),
            ("wall-24", Absent),
            ("wall-25", WrongFile),
            ("wall-30", Failed)
        ]
    );
    let files: Vec<_> = answers.iter().map(|x| x.file).collect();
    assert_eq!(files, [a.file, b.file, c.file, d.file]);
    assert_eq!((&answers[0].found, &answers[0].error), (&None, &None));
    assert_eq!((&answers[1].found, &answers[1].error), (&None, &None));
    assert_eq!(answers[2].found, Some(sha256_hex(b"not clip c")));
    assert_eq!(answers[2].error, None);
    let failed = answers[3].error.as_deref().unwrap();
    assert!(failed.contains("ffmpeg failed"), "{failed}");
    assert_eq!(fake.runs().len(), 3, "a's two runs, d's failed video run");
    let ids: Vec<String> = sqlx::query_scalar("SELECT youtube_id FROM videos")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(ids, ["measure-v01"]);

    let again = Fake {
        fail_on: Some(d.file),
        ..Fake::default()
    };
    let answers = import_listed(&pool, cache.path(), bench.path(), &list, &again).await;
    let outcomes: Vec<_> = answers.iter().map(|x| x.outcome).collect();
    assert_eq!(outcomes, [Already, Absent, WrongFile, Failed]);
    assert_eq!(again.runs().len(), 1, "only d's video run");

    let json = serde_json::to_value(&answers[2]).unwrap();
    assert_eq!(json["outcome"], "wrong_file");
    assert_eq!(json["found"], sha256_hex(b"not clip c"));
    assert!(json.get("error").is_none());
    let json = serde_json::to_value(&answers[1]).unwrap();
    assert_eq!(json["outcome"], "absent");
    assert!(json.get("found").is_none() && json.get("error").is_none());
}
