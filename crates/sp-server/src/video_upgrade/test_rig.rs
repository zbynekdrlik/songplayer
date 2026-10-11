//! #223 S11/S12: a cached song and scripted upgrade steps, shared by the
//! upgrade's and the worker's tests.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sqlx::SqlitePool;

use super::*;
use crate::downloader::format::DownloadedFormat;

pub(crate) fn facts(height: u32, first_ms: u64, duration_ms: u64) -> VideoFacts {
    VideoFacts {
        width: height * 16 / 9,
        height,
        first_ms,
        duration_ms,
        frame_us: 40_000,
        end_decoded: true,
    }
}

pub(crate) fn format_of(height: Option<u32>) -> DownloadedFormat {
    DownloadedFormat {
        format_id: "401".into(),
        codec: Some("av01.0.12M.08".into()),
        width: height.map(|h| h * 16 / 9),
        height,
        fps: Some(25.0),
    }
}

pub(crate) const YT: &str = "PySFfTurafA";

/// A row's id and V35 columns.
pub(crate) type Check = (i64, Option<i64>, Option<String>, Option<i64>);
/// A row's id, V34 format id and height.
pub(crate) type FormatRow = (i64, Option<String>, Option<i64>);

/// A cached song on two playlists (one video, shared files) and one other
/// video, in a temp cache dir.
pub(crate) struct Rig {
    pub(crate) pool: SqlitePool,
    pub(crate) dir: tempfile::TempDir,
    pub(crate) video: PathBuf,
}

impl Rig {
    pub(crate) async fn new() -> Self {
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let video = dir
            .path()
            .join("Song_Artist_PySFfTurafA_normalized_video.mp4");
        let audio = dir
            .path()
            .join("Song_Artist_PySFfTurafA_normalized_audio.flac");
        std::fs::write(&video, "old").unwrap();
        std::fs::write(&audio, "audio").unwrap();
        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'a', 'u1'), (2, 'b', 'u2')",
        )
        .execute(&pool)
        .await
        .unwrap();
        for (id, playlist, yt) in [(10, 1, YT), (11, 2, YT), (12, 1, "otherotheri")] {
            sqlx::query(
                "INSERT INTO videos (id, playlist_id, youtube_id, normalized, file_path, \
                 audio_file_path) VALUES (?, ?, ?, 1, ?, ?)",
            )
            .bind(id)
            .bind(playlist)
            .bind(yt)
            .bind(video.to_str().unwrap())
            .bind(audio.to_str().unwrap())
            .execute(&pool)
            .await
            .unwrap();
        }
        Self { pool, dir, video }
    }

    pub(crate) fn temp(&self) -> PathBuf {
        upgrade_temp(self.dir.path(), YT)
    }

    pub(crate) async fn run(&self, steps: &Fake) -> UpgradeReport {
        run(&self.pool, self.dir.path(), YT, 2160, steps, NOW).await
    }

    /// Every row's V35 columns, by row id.
    pub(crate) async fn checks(&self) -> Vec<Check> {
        sqlx::query_as(
            "SELECT id, video_upgrade_cap, video_upgrade_state, video_upgrade_at \
             FROM videos ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    /// Every row's V34 format id and height, by row id.
    pub(crate) async fn formats(&self) -> Vec<FormatRow> {
        sqlx::query_as("SELECT id, video_format_id, video_height FROM videos ORDER BY id")
            .fetch_all(&self.pool)
            .await
            .unwrap()
    }
}

/// Scripted steps: the cached video reads `old`, the temp `new`; the
/// download writes `new_bytes` into the temp (none = writes nothing).
pub(crate) struct Fake {
    pub(crate) old: Result<VideoFacts, String>,
    pub(crate) new: Result<VideoFacts, String>,
    pub(crate) resolved: Result<DownloadedFormat, String>,
    pub(crate) audio_ms: u64,
    pub(crate) new_bytes: Option<&'static str>,
    /// Each download: its format id and whether the temp existed then.
    pub(crate) downloads: Mutex<Vec<(String, bool)>>,
}

impl Fake {
    pub(crate) fn upgrading() -> Self {
        Self {
            old: Ok(facts(1440, 0, 200_000)),
            new: Ok(facts(2160, 0, 200_000)),
            resolved: Ok(format_of(Some(2160))),
            audio_ms: 200_000,
            new_bytes: Some("new"),
            downloads: Mutex::new(Vec::new()),
        }
    }
}

impl Steps for Fake {
    async fn resolve(&self, youtube_id: &str, cap: u32) -> Result<DownloadedFormat, String> {
        assert_eq!((youtube_id, cap), (YT, 2160));
        self.resolved.clone()
    }
    async fn facts(&self, path: &Path) -> Result<VideoFacts, String> {
        let name = path.file_name().unwrap().to_str().unwrap();
        if name.ends_with("_video_upgrade_temp.mp4") {
            self.new.clone()
        } else {
            self.old.clone()
        }
    }
    async fn download(
        &self,
        youtube_id: &str,
        format_id: &str,
        out: &Path,
    ) -> Result<Option<DownloadedFormat>, String> {
        assert_eq!(youtube_id, YT);
        self.downloads
            .lock()
            .unwrap()
            .push((format_id.to_string(), out.exists()));
        if let Some(bytes) = self.new_bytes {
            std::fs::write(out, bytes).unwrap();
        }
        Ok(Some(DownloadedFormat {
            format_id: "401".into(),
            codec: Some("av01.0.12M.08".into()),
            width: Some(3840),
            height: Some(2160),
            fps: Some(24.0),
        }))
    }
    async fn audio_ms(&self, path: &Path) -> Result<u64, String> {
        assert!(path.to_str().unwrap().ends_with("_audio.flac"));
        Ok(self.audio_ms)
    }
}

pub(crate) const NOW: i64 = 1_760_000_000_000;

pub(crate) fn read_to_string(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}
