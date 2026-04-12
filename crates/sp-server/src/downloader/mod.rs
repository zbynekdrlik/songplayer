//! Download worker — orchestrates yt-dlp downloads, metadata extraction,
//! and FFmpeg normalization for queued videos.
//!
//! The FLAC pipeline issues two separate yt-dlp invocations per video —
//! one for the video stream, one for the audio stream. Both are stream
//! copies from YouTube's native encodings; there is no merge step. The
//! audio is then normalized to FLAC by [`normalize::normalize_audio`] and
//! the two resulting sidecar files live alongside each other in the
//! cache directory.

pub mod cache;
pub mod normalize;
pub mod tools;

use crate::metadata::MetadataProvider;
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use tokio::sync::broadcast;
use tools::ToolPaths;

/// Apply platform-specific flags to hide console windows on Windows.
/// All subprocess calls (yt-dlp, ffmpeg) must use this to avoid flashing
/// cmd windows on the desktop.
pub fn hide_console_window(cmd: &mut tokio::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let _ = cmd; // suppress unused warning on non-Windows
}

/// Maximum video resolution height for downloads.
const MAX_RESOLUTION: u32 = 1440;

/// Download timeout in seconds.
const DOWNLOAD_TIMEOUT: u64 = 600;

/// Background worker that downloads, extracts metadata, and normalizes videos.
pub struct DownloadWorker {
    pool: SqlitePool,
    tools: ToolPaths,
    cache_dir: PathBuf,
    providers: Vec<Box<dyn MetadataProvider>>,
    event_tx: broadcast::Sender<String>,
}

impl DownloadWorker {
    pub fn new(
        pool: SqlitePool,
        tools: ToolPaths,
        cache_dir: PathBuf,
        providers: Vec<Box<dyn MetadataProvider>>,
        event_tx: broadcast::Sender<String>,
    ) -> Self {
        Self {
            pool,
            tools,
            cache_dir,
            providers,
            event_tx,
        }
    }

    /// Run the download worker loop until shutdown is signalled.
    pub async fn run(self, mut shutdown: broadcast::Receiver<()>) {
        tracing::info!("download worker started");
        loop {
            tokio::select! {
                _ = shutdown.recv() => {
                    tracing::info!("download worker received shutdown signal");
                    break;
                }
                _ = self.process_next() => {}
            }
            tokio::select! {
                _ = shutdown.recv() => break,
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
            }
        }
        tracing::info!("download worker stopped");
    }

    /// Try to process the next un-normalized video.
    async fn process_next(&self) -> bool {
        let row = match self.fetch_next_unprocessed().await {
            Ok(Some(r)) => r,
            Ok(None) => return false,
            Err(e) => {
                tracing::error!("failed to fetch next video: {e}");
                return false;
            }
        };

        tracing::info!(video_id = %row.youtube_id, title = %row.title, "processing video");
        let _ = self
            .event_tx
            .send(format!("downloading:{}", row.youtube_id));

        let video_temp = self
            .cache_dir
            .join(format!("{}_video_temp.mp4", row.youtube_id));
        // yt-dlp picks the native extension for audio (%(ext)s), so we use
        // a base path and then find the actual file after the call.
        let audio_temp_base = self
            .cache_dir
            .join(format!("{}_audio_temp", row.youtube_id));

        if let Err(e) = self
            .download_video_stream(&row.youtube_id, &video_temp)
            .await
        {
            tracing::error!(video_id = %row.youtube_id, "video download failed: {e}");
            cleanup_temps(&video_temp, &self.cache_dir, &row.youtube_id);
            return false;
        }

        let audio_temp = match self
            .download_audio_stream(&row.youtube_id, &audio_temp_base)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(video_id = %row.youtube_id, "audio download failed: {e}");
                cleanup_temps(&video_temp, &self.cache_dir, &row.youtube_id);
                return false;
            }
        };

        let meta =
            crate::metadata::get_metadata(&self.providers, &row.youtube_id, &row.title).await;

        let video_final = self.cache_dir.join(cache::video_filename(
            &meta.song,
            &meta.artist,
            &row.youtube_id,
            meta.gemini_failed,
        ));
        let audio_final = self.cache_dir.join(cache::audio_filename(
            &meta.song,
            &meta.artist,
            &row.youtube_id,
            meta.gemini_failed,
        ));

        // Normalize audio first — failure here is recoverable.
        if let Err(e) =
            normalize::normalize_audio(&self.tools.ffmpeg, &audio_temp, &audio_final).await
        {
            tracing::error!(video_id = %row.youtube_id, "normalization failed: {e}");
            let _ = tokio::fs::remove_file(&audio_temp).await;
            let _ = tokio::fs::remove_file(&video_temp).await;
            return false;
        }

        // Move the video temp to its final pair name.
        if let Err(e) = tokio::fs::rename(&video_temp, &video_final).await {
            tracing::error!(video_id = %row.youtube_id, "video rename failed: {e}");
            let _ = tokio::fs::remove_file(&audio_final).await;
            let _ = tokio::fs::remove_file(&video_temp).await;
            return false;
        }

        // Drop the audio temp.
        let _ = tokio::fs::remove_file(&audio_temp).await;

        if let Err(e) = crate::db::models::mark_video_processed_pair(
            &self.pool,
            row.id,
            &meta.song,
            &meta.artist,
            meta.source.as_str(),
            meta.gemini_failed,
            video_final.to_string_lossy().as_ref(),
            audio_final.to_string_lossy().as_ref(),
        )
        .await
        {
            tracing::error!(video_id = %row.youtube_id, "DB update failed: {e}");
            return false;
        }

        let _ = self.event_tx.send(format!("processed:{}", row.youtube_id));
        tracing::info!(video_id = %row.youtube_id, "video processed successfully");
        true
    }

    /// Fetch the next video that needs processing.
    async fn fetch_next_unprocessed(&self) -> Result<Option<VideoRow>, sqlx::Error> {
        let row = sqlx::query_as::<_, VideoRow>(
            "SELECT v.id, v.youtube_id, COALESCE(v.title, '') as title
             FROM videos v
             JOIN playlists p ON p.id = v.playlist_id
             WHERE v.normalized = 0 AND p.is_active = 1
             ORDER BY v.id
             LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Download the video stream only via yt-dlp.
    async fn download_video_stream(
        &self,
        video_id: &str,
        output: &Path,
    ) -> Result<(), anyhow::Error> {
        let url = format!("https://www.youtube.com/watch?v={video_id}");
        let format_spec = format!("bestvideo[height<={MAX_RESOLUTION}]");
        let ffmpeg_dir = self
            .tools
            .ffmpeg
            .parent()
            .unwrap_or(std::path::Path::new("."));

        let mut cmd = tokio::process::Command::new(&self.tools.ytdlp);
        cmd.args(["--progress", "--newline"])
            .args(["-f", &format_spec])
            .args(["--ffmpeg-location"])
            .arg(ffmpeg_dir)
            .args(["--js-runtimes", "node"])
            .args(["--socket-timeout", &DOWNLOAD_TIMEOUT.to_string()])
            .args(["--remux-video", "mp4"])
            .arg("--no-part")
            .args(["-o"])
            .arg(output)
            .arg(&url)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        hide_console_window(&mut cmd);
        let child_output = cmd.output().await?;

        if !child_output.status.success() {
            let stderr = String::from_utf8_lossy(&child_output.stderr);
            anyhow::bail!(
                "yt-dlp (video) exited with {}: {}",
                child_output.status,
                stderr
            );
        }
        Ok(())
    }

    /// Download the best audio stream only via yt-dlp. Returns the actual
    /// file path that yt-dlp wrote (the extension is codec-dependent).
    ///
    /// Uses `--print after_move:filepath` so yt-dlp itself reports the
    /// final path on stdout, avoiding a racy directory scan.
    async fn download_audio_stream(
        &self,
        video_id: &str,
        output_base: &Path,
    ) -> Result<PathBuf, anyhow::Error> {
        let url = format!("https://www.youtube.com/watch?v={video_id}");
        let ffmpeg_dir = self
            .tools
            .ffmpeg
            .parent()
            .unwrap_or(std::path::Path::new("."));

        let output_template = format!("{}.%(ext)s", output_base.display());

        let mut cmd = tokio::process::Command::new(&self.tools.ytdlp);
        cmd.args(["--progress", "--newline"])
            .args(["-f", "bestaudio"])
            .args(["--ffmpeg-location"])
            .arg(ffmpeg_dir)
            .args(["--js-runtimes", "node"])
            .args(["--socket-timeout", &DOWNLOAD_TIMEOUT.to_string()])
            .arg("--no-part")
            .args(["--print", "after_move:filepath"])
            .args(["-o", &output_template])
            .arg(&url)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        hide_console_window(&mut cmd);
        let child_output = cmd.output().await?;

        if !child_output.status.success() {
            let stderr = String::from_utf8_lossy(&child_output.stderr);
            anyhow::bail!(
                "yt-dlp (audio) exited with {}: {}",
                child_output.status,
                stderr
            );
        }

        // `--print after_move:filepath` writes the final path as the
        // last non-empty line on stdout.
        let stdout = String::from_utf8_lossy(&child_output.stdout);
        let filepath = stdout
            .lines()
            .rev()
            .find(|l| !l.is_empty() && !l.starts_with('['))
            .map(|l| PathBuf::from(l.trim()))
            .filter(|p| p.exists())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "yt-dlp did not report a valid filepath for audio of {video_id}; stdout: {stdout}"
                )
            })?;

        Ok(filepath)
    }
}

fn cleanup_temps(video_temp: &Path, cache_dir: &Path, video_id: &str) {
    let _ = std::fs::remove_file(video_temp);
    // Remove any audio temp file with a matching prefix.
    let prefix = format!("{video_id}_audio_temp");
    if let Ok(entries) = std::fs::read_dir(cache_dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str()
                && name.starts_with(&prefix)
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

#[derive(Debug, sqlx::FromRow)]
struct VideoRow {
    id: i64,
    youtube_id: String,
    title: String,
}
