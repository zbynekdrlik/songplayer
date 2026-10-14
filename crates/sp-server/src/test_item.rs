//! #228: the local test item — camera-box's measurement clip, played on
//! `SP-program` like any playlist, for camera-box's CG-segment protocol.
//!
//! Music must never reach YouTube from a test run (the owner, #228), so the
//! test content is camera-box's synthesized measurement clip
//! (`measurement-clip-v1-128s.mp4`, camera-box issue 1404: 1080p30 H.264, a
//! per-frame dual QR under run id 911016, 48 kHz stereo AAC with a QPSK
//! marker every 0.5 s over a −30 dBFS 1 kHz bed; 128 s, so it may loop).
//!
//! - **The playlist.** One playlist of kind [`TEST_KIND`] (`kind` is free
//!   text, no schema change): never synced (startup sync takes `youtube`
//!   playlists only), mode `loop`, NDI name [`TEST_NDI_NAME`] (its scene
//!   `sp-test`, pressed through the facade like any playlist's), one video
//!   [`TEST_YOUTUBE_ID`] (`metadata_source = 'manual'`: never repaired).
//! - **The import** ([`import`]): the clip is read from the box's sample dir
//!   (`<data dir>/bench`, the decode bench's), its sha256 checked against
//!   [`TEST_CLIP_SHA256`] (any other file is refused, nothing written), then
//!   split into the cache's layout by two ffmpeg runs into temp names: the
//!   video stream COPIED ([`video_args`]), the audio decoded into FLAC with
//!   NO filter, NO resampling and NO gain ([`audio_args`]), so the −30 dBFS
//!   bed and the markers reach `SP-program` as the clip carries them. Both
//!   are renamed under `cache::SONG_FILES` and the row is upserted. A row
//!   that already records both files on disk is left as it is (`already`).
//! - **Never processed.** `not_test_item!` is the ONE SQL fragment every
//!   worker queue (lyrics, stems, dub, metadata repair, download) and its
//!   dashboard count adds: no worker takes a test item, no paid AI is asked.
//! - **Not an operator playlist.** `GET /api/v1/playlists` leaves it out
//!   (the dashboard, the Program control and the Live page never list it);
//!   `/api/v1/test-item` serves its ids, start and stop.
//! - **#272: a pinned clip LIST** (`test_item_clips.rs`): camera-box's clip
//!   (the default) and SongPlayer's wall measurement clips, each a row of
//!   the same test playlist, each imported by the steps above under its own
//!   names ([`Clip`]).

use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;

use serde::Serialize;
use sqlx::{Row, SqlitePool};

use crate::downloader::cache;

#[path = "test_item_clips.rs"]
mod clips;
pub use clips::{
    CAMERA_BOX, CLIPS, Clip, WALL_24, WALL_25, WALL_30, WALL_ARTIST, WALL_DURATION_MS, clip_named,
};

/// The playlist kind of the test item.
pub const TEST_KIND: &str = "test";

/// The test playlist's name (hidden from the operator's lists).
pub const TEST_PLAYLIST_NAME: &str = "Test: meracie video (camera-box)";

/// The test playlist's NDI output name: its scene label (`sp-test`).
pub const TEST_NDI_NAME: &str = "SP-test";

/// The test item's id: 11 characters, as the cache's file names need.
pub const TEST_YOUTUBE_ID: &str = "measure-v01";

/// The song and artist its files are named after, and its title.
pub const TEST_SONG: &str = "Meracie video v1";
pub const TEST_ARTIST: &str = "camera-box";
pub const TEST_TITLE: &str = "camera-box meracie video v1 (128 s)";

/// The clip's length, ms.
pub const TEST_DURATION_MS: i64 = 128_000;

/// The clip's file name as camera-box delivers it.
pub const TEST_CLIP_FILE: &str = "measurement-clip-v1-128s.mp4";

/// The sha256 of camera-box's `measurement-clip-v1-128s.mp4` (20 784 509
/// bytes, camera-box issue 1404 Task 5, re-checked on dev1 8.10.2026).
pub const TEST_CLIP_SHA256: [u8; 32] = [
    0xa0, 0x11, 0x8a, 0xd7, 0x74, 0x19, 0x12, 0x53, 0xb7, 0xda, 0xd8, 0x8f, 0x45, 0x48, 0xba, 0x7e,
    0x89, 0xa9, 0x97, 0x5f, 0x64, 0x66, 0x78, 0x2b, 0x08, 0x94, 0x24, 0xa0, 0xf7, 0x70, 0xa7, 0x48,
];

/// The ONE SQL fragment that keeps a test item out of a worker queue: its
/// row's playlist is not a test playlist. The column is unqualified, so it
/// reads the innermost `videos` in scope — a bare `videos` query, a
/// `videos v JOIN playlists p` one (playlists has no `playlist_id`), and a
/// correlated `videos s` subquery alike. A macro, so a `concat!` const can
/// carry it; [`NOT_TEST_ITEM`] is its text.
macro_rules! not_test_item {
    () => {
        "playlist_id NOT IN (SELECT id FROM playlists WHERE kind = 'test')"
    };
}
pub(crate) use not_test_item;

/// The text of `not_test_item!`.
pub const NOT_TEST_ITEM: &str = not_test_item!();

/// `bytes` as lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The pinned clip's sha256, as `peer::hash::sha256_file` writes it.
pub fn clip_sha256() -> String {
    hex(&TEST_CLIP_SHA256)
}

/// The test item as `/api/v1/test-item` serves it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TestItem {
    pub playlist_id: i64,
    pub video_id: i64,
    pub youtube_id: String,
    pub ndi_output_name: String,
    /// The scene a facade press names (the scene catalog's rule: the NDI
    /// name, lowercased).
    pub scene: String,
    pub duration_ms: Option<i64>,
}

/// The imported test item — camera-box's clip, the default (its playlist
/// and its downloaded video) — or `None` before its import.
pub async fn find(pool: &SqlitePool) -> Result<Option<TestItem>, sqlx::Error> {
    find_clip(pool, &CAMERA_BOX).await
}

/// `clip`'s imported row in the test playlist, or `None` before its import.
pub async fn find_clip(pool: &SqlitePool, clip: &Clip) -> Result<Option<TestItem>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT p.id AS playlist_id, p.ndi_output_name, v.id AS video_id, v.duration_ms \
         FROM playlists p JOIN videos v ON v.playlist_id = p.id \
         WHERE p.kind = ? AND v.youtube_id = ? AND v.normalized = 1 \
         ORDER BY p.id LIMIT 1",
    )
    .bind(TEST_KIND)
    .bind(clip.youtube_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| {
        let ndi_output_name: String = r.get("ndi_output_name");
        TestItem {
            playlist_id: r.get("playlist_id"),
            video_id: r.get("video_id"),
            youtube_id: clip.youtube_id.to_string(),
            scene: ndi_output_name.to_ascii_lowercase(),
            ndi_output_name,
            duration_ms: r.get("duration_ms"),
        }
    }))
}

/// The first listed clip that is imported (any of them puts the test
/// playlist on the box), or `None` while none is.
pub async fn find_any(pool: &SqlitePool) -> Result<Option<TestItem>, sqlx::Error> {
    for clip in &CLIPS {
        if let Some(item) = find_clip(pool, clip).await? {
            return Ok(Some(item));
        }
    }
    Ok(None)
}

/// The test playlist's id, made when there is none: kind `test`, mode
/// `loop`, active, no YouTube URL. An existing one is left as it is.
pub async fn ensure_playlist(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    sqlx::query(
        "INSERT INTO playlists (name, youtube_url, ndi_output_name, playback_mode, is_active, kind) \
         SELECT ?, '', ?, 'loop', 1, ? \
         WHERE NOT EXISTS (SELECT 1 FROM playlists WHERE kind = ?)",
    )
    .bind(TEST_PLAYLIST_NAME)
    .bind(TEST_NDI_NAME)
    .bind(TEST_KIND)
    .bind(TEST_KIND)
    .execute(pool)
    .await?;
    sqlx::query_scalar("SELECT id FROM playlists WHERE kind = ? ORDER BY id LIMIT 1")
        .bind(TEST_KIND)
        .fetch_one(pool)
        .await
}

/// Where `clip`'s video and audio live in the cache: the split layout's
/// names (`cache::video_filename` / `audio_filename`).
pub fn final_paths(cache_dir: &Path, clip: &Clip) -> (PathBuf, PathBuf) {
    let (song, artist, id) = (clip.song, clip.artist, clip.youtube_id);
    (
        cache_dir.join(cache::video_filename(song, artist, id, false)),
        cache_dir.join(cache::audio_filename(song, artist, id, false)),
    )
}

/// The temp names ffmpeg writes `clip` into (in the cache, so the rename
/// never crosses a volume; no cache scan reads them as a song's file).
pub fn temp_paths(cache_dir: &Path, clip: &Clip) -> (PathBuf, PathBuf) {
    let id = clip.youtube_id;
    (
        cache_dir.join(format!("test-item-{id}.importing-video")),
        cache_dir.join(format!("test-item-{id}.importing-audio")),
    )
}

/// ffmpeg's common start: no stdin, quiet, the clip as its one input.
fn input_args(clip: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["-nostdin", "-hide_banner", "-loglevel", "error", "-i"]
        .map(OsString::from)
        .to_vec();
    args.push(clip.into());
    args
}

/// The video: the clip's first video stream COPIED into an MP4 (no
/// re-encode), no audio, subtitle or data stream.
pub fn video_args(clip: &Path, out: &Path) -> Vec<OsString> {
    let mut args = input_args(clip);
    args.extend(
        [
            "-map", "0:v:0", "-c:v", "copy", "-an", "-sn", "-dn", "-f", "mp4", "-y",
        ]
        .map(OsString::from),
    );
    args.push(out.into());
    args
}

/// The audio: the clip's first audio stream decoded and stored as FLAC —
/// no filter, no resampling, no channel change, no gain (`s32`: the
/// decoder's samples at 24 bits). The pinned clip is 48 kHz stereo, what
/// the program carries.
pub fn audio_args(clip: &Path, out: &Path) -> Vec<OsString> {
    let mut args = input_args(clip);
    args.extend(
        [
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
        ]
        .map(OsString::from),
    );
    args.push(out.into());
    args
}

/// A transcode run's future: `Err` carries why it failed.
pub type TranscodeRun<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// What runs ffmpeg ([`Ffmpeg`] in production; the tests write the files).
pub trait Transcoder: Send + Sync {
    fn run(&self, args: Vec<OsString>) -> TranscodeRun<'_>;
}

/// The box's ffmpeg (`ToolPaths::ffmpeg`).
pub struct Ffmpeg(pub PathBuf);

impl Transcoder for Ffmpeg {
    /// Runs the real ffmpeg (hidden console): no Linux test has one, the
    /// arguments are [`video_args`] / [`audio_args`]'s (tested).
    #[cfg_attr(test, mutants::skip)]
    fn run(&self, args: Vec<OsString>) -> TranscodeRun<'_> {
        Box::pin(async move {
            let mut cmd = tokio::process::Command::new(&self.0);
            cmd.args(&args).stdout(Stdio::null()).stderr(Stdio::piped());
            crate::downloader::hide_console_window(&mut cmd);
            let out = cmd
                .output()
                .await
                .map_err(|e| format!("ffmpeg did not start: {e}"))?;
            if out.status.success() {
                return Ok(());
            }
            let stderr = String::from_utf8_lossy(&out.stderr);
            Err(format!("ffmpeg failed ({}): {}", out.status, stderr.trim()))
        })
    }
}

/// How an import ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportOutcome {
    /// The files were made and the row recorded.
    Imported,
    /// The row already recorded both files on disk: nothing was written.
    Already,
}

/// Why an import wrote nothing (or, past the transcode, not all of it).
#[derive(Debug)]
pub enum ImportError {
    /// The file is none of the clips it may be: its sha256 (hex).
    WrongFile(String),
    /// The file could not be read.
    Read(std::io::Error),
    /// ffmpeg failed.
    Transcode(String),
    /// A file could not be put under its cache name.
    Place(std::io::Error),
    /// The store failed.
    Store(sqlx::Error),
}

impl std::fmt::Display for ImportError {
    #[cfg_attr(test, mutants::skip)] // the texts are for the log and the HTTP body
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongFile(found) => write!(f, "not the measurement clip (sha256 {found})"),
            Self::Read(e) => write!(f, "reading the clip failed: {e}"),
            Self::Transcode(why) => write!(f, "{why}"),
            Self::Place(e) => write!(f, "placing the files in the cache failed: {e}"),
            Self::Store(e) => write!(f, "the store failed: {e}"),
        }
    }
}

/// Whether `clip`'s test row already records both files under their names,
/// and both are on disk.
async fn is_imported(
    pool: &SqlitePool,
    playlist_id: i64,
    clip: &Clip,
    video: &Path,
    audio: &Path,
) -> Result<bool, sqlx::Error> {
    let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT file_path, audio_file_path FROM videos \
         WHERE playlist_id = ? AND youtube_id = ? AND normalized = 1",
    )
    .bind(playlist_id)
    .bind(clip.youtube_id)
    .fetch_optional(pool)
    .await?;
    let records = |column: &Option<String>, path: &Path| {
        column.as_deref() == Some(&*path.to_string_lossy()) && path.is_file()
    };
    Ok(row.is_some_and(|(v, a)| records(&v, video) && records(&a, audio)))
}

/// Record `clip`'s row: downloaded, its files, its manual title.
async fn record(
    pool: &SqlitePool,
    playlist_id: i64,
    clip: &Clip,
    video: &Path,
    audio: &Path,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, title, song, artist, metadata_source, \
             gemini_failed, duration_ms, file_path, audio_file_path, normalized) \
         VALUES (?, ?, ?, ?, ?, 'manual', 0, ?, ?, ?, 1) \
         ON CONFLICT(playlist_id, youtube_id) DO UPDATE SET \
             title = excluded.title, song = excluded.song, artist = excluded.artist, \
             metadata_source = 'manual', gemini_failed = 0, duration_ms = excluded.duration_ms, \
             file_path = excluded.file_path, audio_file_path = excluded.audio_file_path, \
             normalized = 1 \
         RETURNING id",
    )
    .bind(playlist_id)
    .bind(clip.youtube_id)
    .bind(clip.title)
    .bind(clip.song)
    .bind(clip.artist)
    .bind(clip.duration_ms)
    .bind(video.to_string_lossy().into_owned())
    .bind(audio.to_string_lossy().into_owned())
    .fetch_one(pool)
    .await
}

/// Remove a temp file; one that is not there is fine.
pub(crate) fn remove_temp(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// The two ffmpeg runs of `file` into `clip`'s temp names, then (under
/// `cache::SONG_FILES`) both renamed to its cache names and its row
/// recorded.
async fn transcode_and_place(
    pool: &SqlitePool,
    playlist_id: i64,
    clip: &Clip,
    file: &Path,
    temps: &(PathBuf, PathBuf),
    finals: &(PathBuf, PathBuf),
    transcoder: &dyn Transcoder,
) -> Result<(), ImportError> {
    let (video_tmp, audio_tmp) = temps;
    let (video, audio) = finals;
    transcoder
        .run(video_args(file, video_tmp))
        .await
        .map_err(ImportError::Transcode)?;
    transcoder
        .run(audio_args(file, audio_tmp))
        .await
        .map_err(ImportError::Transcode)?;
    let _files = cache::SONG_FILES.lock().await;
    std::fs::rename(video_tmp, video).map_err(ImportError::Place)?;
    std::fs::rename(audio_tmp, audio).map_err(ImportError::Place)?;
    record(pool, playlist_id, clip, video, audio)
        .await
        .map_err(ImportError::Store)?;
    Ok(())
}

/// Import the file at `file` as the listed clip whose sha256 it has (see
/// the module doc): `clips` are the ones it may be (production:
/// [`CLIPS`]; one clip for the list import). The temp files are removed
/// whatever the transcode did (a removal that fails is WARNed).
pub async fn import(
    pool: &SqlitePool,
    cache_dir: &Path,
    file: &Path,
    clips: &[Clip],
    transcoder: &dyn Transcoder,
) -> Result<(ImportOutcome, Clip, TestItem), ImportError> {
    let found = crate::peer::hash::sha256_file(file, 0)
        .await
        .map_err(ImportError::Read)?;
    let Some(clip) = clips.iter().find(|clip| clip.sha256_hex() == found) else {
        return Err(ImportError::WrongFile(found));
    };
    let playlist_id = ensure_playlist(pool).await.map_err(ImportError::Store)?;
    let finals = final_paths(cache_dir, clip);
    let outcome = if is_imported(pool, playlist_id, clip, &finals.0, &finals.1)
        .await
        .map_err(ImportError::Store)?
    {
        ImportOutcome::Already
    } else {
        let temps = temp_paths(cache_dir, clip);
        let placed =
            transcode_and_place(pool, playlist_id, clip, file, &temps, &finals, transcoder).await;
        for temp in [&temps.0, &temps.1] {
            if let Err(e) = remove_temp(temp) {
                tracing::warn!(%e, path = %temp.display(), "test item: a temp file could not be removed");
            }
        }
        placed?;
        ImportOutcome::Imported
    };
    let item = find_clip(pool, clip)
        .await
        .map_err(ImportError::Store)?
        .ok_or(ImportError::Store(sqlx::Error::RowNotFound))?;
    Ok((outcome, *clip, item))
}

/// What the list import did with one listed clip.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipOutcome {
    /// Its files were made and its row recorded.
    Imported,
    /// Its row already recorded both files on disk.
    Already,
    /// No file of its name in the sample dir: nothing done, no error.
    Absent,
    /// A file of its name that is not it: nothing written.
    WrongFile,
    /// ffmpeg, the cache or the store failed.
    Failed,
}

/// One listed clip in the answer of the list import.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ClipImport {
    pub name: &'static str,
    pub file: &'static str,
    pub outcome: ClipOutcome,
    /// `wrong_file`: the sha256 of the file found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub found: Option<String>,
    /// `failed`: why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Import every clip of `clips` whose file is in `bench_dir` (its own file
/// name, its own sha256), in order; a clip whose file is not there is
/// `absent`, never an error. Each clip is imported as [`import`] does.
pub async fn import_listed(
    pool: &SqlitePool,
    cache_dir: &Path,
    bench_dir: &Path,
    clips: &[Clip],
    transcoder: &dyn Transcoder,
) -> Vec<ClipImport> {
    let mut answers = Vec::with_capacity(clips.len());
    for clip in clips {
        let path = bench_dir.join(clip.file);
        let mut answer = ClipImport {
            name: clip.name,
            file: clip.file,
            outcome: ClipOutcome::Absent,
            found: None,
            error: None,
        };
        if path.is_file() {
            let one = std::slice::from_ref(clip);
            match import(pool, cache_dir, &path, one, transcoder).await {
                Ok((ImportOutcome::Imported, ..)) => answer.outcome = ClipOutcome::Imported,
                Ok((ImportOutcome::Already, ..)) => answer.outcome = ClipOutcome::Already,
                Err(ImportError::WrongFile(found)) => {
                    answer.outcome = ClipOutcome::WrongFile;
                    answer.found = Some(found);
                }
                Err(e) => {
                    answer.outcome = ClipOutcome::Failed;
                    answer.error = Some(e.to_string());
                }
            }
        }
        answers.push(answer);
    }
    answers
}

#[cfg(test)]
#[path = "test_item_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "test_item_tests_queues.rs"]
mod tests_queues;
