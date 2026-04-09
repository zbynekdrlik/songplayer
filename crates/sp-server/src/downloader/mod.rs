//! Download worker — orchestrates yt-dlp downloads, metadata extraction,
//! and FFmpeg normalization for queued videos.

pub mod cache;
pub mod normalize;
pub mod tools;

use crate::metadata::MetadataProvider;
use sqlx::SqlitePool;
use std::path::PathBuf;
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
    ///
    /// The worker polls the database for un-normalized videos belonging to
    /// active playlists and processes them one at a time.
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

            // Brief pause before polling again.
            tokio::select! {
                _ = shutdown.recv() => break,
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
            }
        }

        tracing::info!("download worker stopped");
    }

    /// Try to process the next un-normalized video.
    ///
    /// Returns `true` if a video was processed, `false` if the queue is empty.
    async fn process_next(&self) -> bool {
        let row = match self.fetch_next_unprocessed().await {
            Ok(Some(r)) => r,
            Ok(None) => return false,
            Err(e) => {
                tracing::error!("failed to fetch next video: {e}");
                return false;
            }
        };

        tracing::info!(
            video_id = %row.youtube_id,
            title = %row.title,
            "processing video"
        );

        let _ = self
            .event_tx
            .send(format!("downloading:{}", row.youtube_id));

        // Step 1: Download.
        let temp_path = self.cache_dir.join(format!("{}_temp.mp4", row.youtube_id));
        if let Err(e) = self.download_video(&row.youtube_id, &temp_path).await {
            tracing::error!(video_id = %row.youtube_id, "download failed: {e}");
            return false;
        }

        // Step 2: Extract metadata.
        let meta =
            crate::metadata::get_metadata(&self.providers, &row.youtube_id, &row.title).await;

        // Step 3: Normalize audio.
        let out_name = cache::normalized_filename(
            &meta.song,
            &meta.artist,
            &row.youtube_id,
            meta.gemini_failed,
        );
        let output_path = self.cache_dir.join(&out_name);

        if let Err(e) =
            normalize::normalize_audio(&self.tools.ffmpeg, &temp_path, &output_path).await
        {
            tracing::error!(video_id = %row.youtube_id, "normalization failed: {e}");
            // Clean up temp file on failure.
            let _ = tokio::fs::remove_file(&temp_path).await;
            return false;
        }

        // Clean up temp file.
        let _ = tokio::fs::remove_file(&temp_path).await;

        // Step 4: Update DB.
        if let Err(e) = self
            .mark_video_processed(
                row.id,
                &meta.song,
                &meta.artist,
                meta.source.as_str(),
                meta.gemini_failed,
                output_path.to_string_lossy().as_ref(),
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

    /// Download a video using yt-dlp.
    async fn download_video(
        &self,
        video_id: &str,
        output: &std::path::Path,
    ) -> Result<(), anyhow::Error> {
        let url = format!("https://www.youtube.com/watch?v={video_id}");
        // Prefer H.264 (avc1) which Windows Media Foundation always supports.
        // AV1/VP9 require optional codec extensions. Fall back to any codec if
        // H.264 is unavailable for the requested resolution.
        let format_spec = format!(
            "bestvideo[height<={MAX_RESOLUTION}][vcodec^=avc1]+bestaudio/\
             bestvideo[height<={MAX_RESOLUTION}]+bestaudio/\
             best[height<={MAX_RESOLUTION}]"
        );

        // yt-dlp needs to know where ffmpeg is for merging video+audio streams.
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
            .args(["--merge-output-format", "mp4"])
            .args(["-o"])
            .arg(output)
            .arg(&url)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        hide_console_window(&mut cmd);
        let child_output = cmd.output().await?;

        if !child_output.status.success() {
            let stderr = String::from_utf8_lossy(&child_output.stderr);
            anyhow::bail!("yt-dlp exited with {}: {}", child_output.status, stderr);
        }

        Ok(())
    }

    /// Mark a video as processed in the database.
    async fn mark_video_processed(
        &self,
        video_db_id: i64,
        song: &str,
        artist: &str,
        metadata_source: &str,
        gemini_failed: bool,
        file_path: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE videos
             SET song = ?, artist = ?, metadata_source = ?,
                 gemini_failed = ?, file_path = ?, normalized = 1
             WHERE id = ?",
        )
        .bind(song)
        .bind(artist)
        .bind(metadata_source)
        .bind(gemini_failed as i32)
        .bind(file_path)
        .bind(video_db_id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}

/// Lightweight row struct for the download worker's query.
#[derive(Debug, sqlx::FromRow)]
struct VideoRow {
    id: i64,
    youtube_id: String,
    title: String,
}
