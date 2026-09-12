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
use std::ffi::OsString;
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

/// yt-dlp format selector. Uses `/` fallback chain so yt-dlp walks each
/// option in order and picks the first one YouTube actually serves.
///
/// Order:
/// 1. AV1 MP4 (highest quality per byte, decoded fine by MF's AV1 transform).
/// 2. H.264 via HLS (`protocol*=m3u8`). Different encoder/muxer path than
///    DASH. Some H.264 1080p DASH variants (observed on `xrhVLX6vwPk` THE
///    DEEP on 2026-04-23) generate an SPS that Windows Media Foundation's
///    hardware transform rejects — every `ReadSample` returns EOS on the
///    first call, producing `frame_count=0`. The HLS 1080p MP4 for the same
///    video is a distinct encode that MF decodes cleanly.
/// 3. Plain `bestvideo` as last-resort.
fn format_spec() -> String {
    format!(
        "bv*[height<={max}][vcodec^=av01]/\
         bv*[height<={max}][protocol*=m3u8][vcodec^=avc1]/\
         bv*[height<={max}]",
        max = MAX_RESOLUTION
    )
}

/// Download timeout in seconds.
const DOWNLOAD_TIMEOUT: u64 = 600;

/// Build the argument list for the video-stream yt-dlp invocation.
///
/// Reproduces the fixed flag set exactly, and — when `cookies` is
/// `Some` — inserts a `--cookies <path>` pair immediately before the
/// URL (#141: an anonymous request now gets YouTube's "Sign in to
/// confirm you're not a bot" wall; a verified Netscape cookie file
/// clears it).
pub(crate) fn ytdlp_video_args(
    format_spec: &str,
    ffmpeg_dir: &Path,
    output: &Path,
    url: &str,
    cookies: Option<&Path>,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "--progress".into(),
        "--newline".into(),
        "-f".into(),
        format_spec.into(),
        "--ffmpeg-location".into(),
        ffmpeg_dir.into(),
        "--js-runtimes".into(),
        "node".into(),
        "--socket-timeout".into(),
        DOWNLOAD_TIMEOUT.to_string().into(),
        "--remux-video".into(),
        "mp4".into(),
        "--no-part".into(),
        "-o".into(),
        output.into(),
    ];
    if let Some(cookies) = cookies {
        args.push("--cookies".into());
        args.push(cookies.into());
    }
    args.push(url.into());
    args
}

/// Build the argument list for the audio-stream yt-dlp invocation. Same
/// `--cookies` insertion rule as [`ytdlp_video_args`] — see #141.
pub(crate) fn ytdlp_audio_args(
    ffmpeg_dir: &Path,
    output_template: &str,
    url: &str,
    cookies: Option<&Path>,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "--progress".into(),
        "--newline".into(),
        "-f".into(),
        "bestaudio".into(),
        "--ffmpeg-location".into(),
        ffmpeg_dir.into(),
        "--js-runtimes".into(),
        "node".into(),
        "--socket-timeout".into(),
        DOWNLOAD_TIMEOUT.to_string().into(),
        "--no-part".into(),
        "--print".into(),
        "after_move:filepath".into(),
        "-o".into(),
        output_template.into(),
    ];
    if let Some(cookies) = cookies {
        args.push("--cookies".into());
        args.push(cookies.into());
    }
    args.push(url.into());
    args
}

/// Background worker that downloads, extracts metadata, and normalizes videos.
pub struct DownloadWorker {
    pool: SqlitePool,
    tools: ToolPaths,
    cache_dir: PathBuf,
    /// Directory holding the app's data (same directory as the SQLite DB).
    /// A `cookies.txt` Netscape cookie file dropped here — production path
    /// `C:\ProgramData\SongPlayer\cookies.txt` — is passed to yt-dlp on
    /// every download to work around YouTube's anonymous-download bot
    /// check (#141). Re-checked per download, not cached at startup, so a
    /// cookie file added later takes effect without a restart.
    data_dir: PathBuf,
    providers: Vec<Box<dyn MetadataProvider>>,
    event_tx: broadcast::Sender<String>,
}

impl DownloadWorker {
    pub fn new(
        pool: SqlitePool,
        tools: ToolPaths,
        cache_dir: PathBuf,
        data_dir: PathBuf,
        providers: Vec<Box<dyn MetadataProvider>>,
        event_tx: broadcast::Sender<String>,
    ) -> Self {
        Self {
            pool,
            tools,
            cache_dir,
            data_dir,
            providers,
            event_tx,
        }
    }

    /// The cookie file path, if one currently exists on disk. Re-checked on
    /// every call (not cached) so a file dropped in after startup is picked
    /// up on the very next download without requiring a restart.
    fn cookies_path(&self) -> Option<PathBuf> {
        let path = self.data_dir.join("cookies.txt");
        path.exists().then_some(path)
    }

    /// Run the download worker loop until shutdown is signalled.
    pub async fn run(self, mut shutdown: broadcast::Receiver<()>) {
        tracing::info!("download worker started");
        match self.cookies_path() {
            Some(path) => tracing::info!(
                path = %path.display(),
                "yt-dlp cookies file present — authenticated YouTube downloads"
            ),
            None => tracing::warn!(
                path = %self.data_dir.join("cookies.txt").display(),
                "yt-dlp cookies file absent — YouTube downloads may hit the bot-check"
            ),
        }
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
        let format_spec = format_spec();
        let ffmpeg_dir = self
            .tools
            .ffmpeg
            .parent()
            .unwrap_or(std::path::Path::new("."));
        let cookies = self.cookies_path();
        tracing::debug!(
            video_id,
            cookies_attached = cookies.is_some(),
            "building yt-dlp video-stream command"
        );
        let args = ytdlp_video_args(&format_spec, ffmpeg_dir, output, &url, cookies.as_deref());

        let mut cmd = tokio::process::Command::new(&self.tools.ytdlp);
        cmd.args(&args)
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
        let cookies = self.cookies_path();
        tracing::debug!(
            video_id,
            cookies_attached = cookies.is_some(),
            "building yt-dlp audio-stream command"
        );
        let args = ytdlp_audio_args(ffmpeg_dir, &output_template, &url, cookies.as_deref());

        let mut cmd = tokio::process::Command::new(&self.tools.ytdlp);
        cmd.args(&args)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_spec_orders_av1_then_hls_then_dash() {
        let spec = format_spec();
        // AV1 first — highest quality per byte, MF hardware-transform safe.
        let av1_pos = spec.find("vcodec^=av01").expect("AV1 alternative present");
        // HLS H.264 second — different encoder path from DASH, MF-compatible
        // for the THE-DEEP class of broken 1080p DASH encodes.
        let hls_pos = spec
            .find("protocol*=m3u8")
            .expect("HLS alternative present");
        // Unconstrained `bv*` last — plain bestvideo fallback.
        let fallback_pos = spec.rfind("bv*").expect("final fallback present");
        assert!(
            av1_pos < hls_pos,
            "AV1 must precede HLS in the fallback chain"
        );
        assert!(
            hls_pos < fallback_pos,
            "HLS must precede the unconstrained fallback"
        );
    }

    #[test]
    fn format_spec_applies_max_resolution() {
        let spec = format_spec();
        let needle = format!("height<={MAX_RESOLUTION}");
        // Every alternative must cap at MAX_RESOLUTION so we never pull 4K.
        assert_eq!(
            spec.matches(&needle).count(),
            3,
            "each of the 3 alternatives must carry the height cap; spec: {spec}"
        );
    }

    /// RED (#141): yt-dlp answers every anonymous download with "Sign in
    /// to confirm you're not a bot". A verified Netscape cookie file on
    /// disk must be threaded through as `--cookies <path>`, right before
    /// the URL, on both the video and audio yt-dlp invocations.
    #[test]
    fn ytdlp_video_args_appends_cookies_before_url_when_present() {
        let format_spec = "bv*[height<=1440]";
        let ffmpeg_dir = Path::new("/opt/ffmpeg");
        let output = Path::new("/cache/abc_video_temp.mp4");
        let url = "https://www.youtube.com/watch?v=abc";
        let cookies = Path::new("/data/cookies.txt");

        let args = ytdlp_video_args(format_spec, ffmpeg_dir, output, url, Some(cookies));

        let cookies_pos = args
            .iter()
            .position(|a| a.to_str() == Some("--cookies"))
            .expect("--cookies flag present when a cookie file is given");
        assert_eq!(
            args[cookies_pos + 1].to_str(),
            cookies.to_str(),
            "the element right after --cookies must be the cookie file path"
        );
        assert_eq!(
            args.last().unwrap().to_str(),
            Some(url),
            "the URL must remain the last argument even with --cookies inserted"
        );
    }

    #[test]
    fn ytdlp_video_args_omits_cookies_when_absent() {
        let format_spec = "bv*[height<=1440]";
        let ffmpeg_dir = Path::new("/opt/ffmpeg");
        let output = Path::new("/cache/abc_video_temp.mp4");
        let url = "https://www.youtube.com/watch?v=abc";

        let args = ytdlp_video_args(format_spec, ffmpeg_dir, output, url, None);

        assert!(
            !args.iter().any(|a| a.to_str() == Some("--cookies")),
            "no --cookies flag when no cookie file exists"
        );
        assert_eq!(args.last().unwrap().to_str(), Some(url));
    }

    #[test]
    fn ytdlp_video_args_keeps_existing_fixed_flags() {
        let format_spec = "bv*[height<=1440]";
        let ffmpeg_dir = Path::new("/opt/ffmpeg");
        let output = Path::new("/cache/abc_video_temp.mp4");
        let url = "https://www.youtube.com/watch?v=abc";

        let args = ytdlp_video_args(format_spec, ffmpeg_dir, output, url, None);

        for flag in ["-f", "--js-runtimes", "node", "--no-part", "--remux-video"] {
            assert!(
                args.iter().any(|a| a.to_str() == Some(flag)),
                "existing flag {flag} must still be present"
            );
        }
    }

    #[test]
    fn ytdlp_audio_args_appends_cookies_before_url_when_present() {
        let ffmpeg_dir = Path::new("/opt/ffmpeg");
        let output_template = "/cache/abc_audio_temp.%(ext)s";
        let url = "https://www.youtube.com/watch?v=abc";
        let cookies = Path::new("/data/cookies.txt");

        let args = ytdlp_audio_args(ffmpeg_dir, output_template, url, Some(cookies));

        let cookies_pos = args
            .iter()
            .position(|a| a.to_str() == Some("--cookies"))
            .expect("--cookies flag present when a cookie file is given");
        assert_eq!(args[cookies_pos + 1].to_str(), cookies.to_str());
        assert_eq!(args.last().unwrap().to_str(), Some(url));
    }

    #[test]
    fn ytdlp_audio_args_omits_cookies_when_absent() {
        let ffmpeg_dir = Path::new("/opt/ffmpeg");
        let output_template = "/cache/abc_audio_temp.%(ext)s";
        let url = "https://www.youtube.com/watch?v=abc";

        let args = ytdlp_audio_args(ffmpeg_dir, output_template, url, None);

        assert!(!args.iter().any(|a| a.to_str() == Some("--cookies")));
        assert_eq!(args.last().unwrap().to_str(), Some(url));
    }

    #[test]
    fn ytdlp_audio_args_keeps_existing_fixed_flags() {
        let ffmpeg_dir = Path::new("/opt/ffmpeg");
        let output_template = "/cache/abc_audio_temp.%(ext)s";
        let url = "https://www.youtube.com/watch?v=abc";

        let args = ytdlp_audio_args(ffmpeg_dir, output_template, url, None);

        for flag in [
            "-f",
            "bestaudio",
            "--js-runtimes",
            "node",
            "--no-part",
            "--print",
            "after_move:filepath",
        ] {
            assert!(
                args.iter().any(|a| a.to_str() == Some(flag)),
                "existing flag {flag} must still be present"
            );
        }
    }

    // -----------------------------------------------------------------
    // RED (#140): a failed row must back off instead of blocking the
    // whole queue behind it forever.
    // -----------------------------------------------------------------

    #[test]
    fn retry_backoff_is_5_min_at_attempt_1() {
        assert_eq!(retry_backoff(1), std::time::Duration::from_secs(5 * 60));
    }

    #[test]
    fn retry_backoff_is_10_min_at_attempt_2() {
        assert_eq!(retry_backoff(2), std::time::Duration::from_secs(10 * 60));
    }

    #[test]
    fn retry_backoff_is_40_min_at_attempt_4() {
        assert_eq!(retry_backoff(4), std::time::Duration::from_secs(40 * 60));
    }

    #[test]
    fn retry_backoff_caps_at_24h_by_attempt_20() {
        assert_eq!(
            retry_backoff(20),
            std::time::Duration::from_secs(24 * 60 * 60)
        );
    }

    #[test]
    fn retry_backoff_caps_at_24h_without_overflow_at_u32_max() {
        assert_eq!(
            retry_backoff(u32::MAX),
            std::time::Duration::from_secs(24 * 60 * 60)
        );
    }

    async fn seed_pool_with_three_videos() -> (SqlitePool, i64, i64, i64) {
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p', 'u')")
            .execute(&pool)
            .await
            .unwrap();

        let now = chrono::Utc::now();
        let future = (now + chrono::Duration::hours(1)).to_rfc3339();
        let past = (now - chrono::Duration::hours(1)).to_rfc3339();

        // A: due 1h from now — must never be picked while B/C are eligible.
        let id_a: i64 = sqlx::query_scalar(
            "INSERT INTO videos (playlist_id, youtube_id, title, next_attempt_at) \
             VALUES (1, 'video_a', 'A', ?) RETURNING id",
        )
        .bind(&future)
        .fetch_one(&pool)
        .await
        .unwrap();

        // B: never failed (NULL next_attempt_at) — eligible immediately.
        let id_b: i64 = sqlx::query_scalar(
            "INSERT INTO videos (playlist_id, youtube_id, title) \
             VALUES (1, 'video_b', 'B') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        // C: due 1h ago — eligible, but behind B in id order.
        let id_c: i64 = sqlx::query_scalar(
            "INSERT INTO videos (playlist_id, youtube_id, title, next_attempt_at) \
             VALUES (1, 'video_c', 'C', ?) RETURNING id",
        )
        .bind(&past)
        .fetch_one(&pool)
        .await
        .unwrap();

        (pool, id_a, id_b, id_c)
    }

    #[tokio::test]
    async fn fetch_next_unprocessed_skips_rows_not_yet_due_for_retry() {
        let (pool, id_a, id_b, id_c) = seed_pool_with_three_videos().await;

        let row = fetch_next_unprocessed(&pool)
            .await
            .unwrap()
            .expect("B is eligible immediately");
        assert_eq!(
            row.id, id_b,
            "B (NULL next_attempt_at, lowest eligible id) must be picked first"
        );
        assert_ne!(row.id, id_a, "A is not due yet");

        sqlx::query("UPDATE videos SET normalized = 1 WHERE id = ?")
            .bind(id_b)
            .execute(&pool)
            .await
            .unwrap();

        let row = fetch_next_unprocessed(&pool)
            .await
            .unwrap()
            .expect("C became due an hour ago");
        assert_eq!(row.id, id_c, "C (due 1h ago) must be picked next");
        assert_ne!(
            row.id, id_a,
            "A (due 1h from now) must never be picked while C is eligible"
        );
    }

    async fn seed_single_video() -> (SqlitePool, i64) {
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p', 'u')")
            .execute(&pool)
            .await
            .unwrap();
        let video_id: i64 = sqlx::query_scalar(
            "INSERT INTO videos (playlist_id, youtube_id, title) \
             VALUES (1, 'vid', 't') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        (pool, video_id)
    }

    #[tokio::test]
    async fn record_download_failure_increments_attempts_and_schedules_retry_then_success_resets() {
        let (pool, video_id) = seed_single_video().await;
        let before = chrono::Utc::now();

        record_download_failure(&pool, video_id, "yt-dlp exited with 1: boom")
            .await
            .unwrap();

        let attempts: i64 = sqlx::query_scalar("SELECT download_attempts FROM videos WHERE id = ?")
            .bind(video_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        let last_error: Option<String> =
            sqlx::query_scalar("SELECT last_download_error FROM videos WHERE id = ?")
                .bind(video_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let next_attempt_at: Option<String> =
            sqlx::query_scalar("SELECT next_attempt_at FROM videos WHERE id = ?")
                .bind(video_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(attempts, 1, "first failure -> 1 attempt");
        assert_eq!(last_error.as_deref(), Some("yt-dlp exited with 1: boom"));
        let next_attempt_at = next_attempt_at.expect("next_attempt_at must be set on failure");
        let parsed = chrono::DateTime::parse_from_rfc3339(&next_attempt_at).expect("valid RFC3339");
        assert!(
            parsed.to_utc() > before,
            "next_attempt_at must be scheduled in the future"
        );

        record_download_failure(&pool, video_id, "second failure")
            .await
            .unwrap();
        let attempts: i64 = sqlx::query_scalar("SELECT download_attempts FROM videos WHERE id = ?")
            .bind(video_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(attempts, 2, "second failure -> 2 attempts");

        // The reset path: the same UPDATE mark_video_processed_pair issues
        // on success zeroes the three bookkeeping columns back out.
        crate::db::models::mark_video_processed_pair(
            &pool, video_id, "Song", "Artist", "test", false, "/v.mp4", "/a.flac",
        )
        .await
        .unwrap();

        let attempts: i64 = sqlx::query_scalar("SELECT download_attempts FROM videos WHERE id = ?")
            .bind(video_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        let last_error: Option<String> =
            sqlx::query_scalar("SELECT last_download_error FROM videos WHERE id = ?")
                .bind(video_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let next_attempt_at: Option<String> =
            sqlx::query_scalar("SELECT next_attempt_at FROM videos WHERE id = ?")
                .bind(video_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(attempts, 0, "success resets download_attempts to 0");
        assert!(last_error.is_none(), "success resets last_download_error");
        assert!(next_attempt_at.is_none(), "success resets next_attempt_at");
    }

    #[tokio::test]
    async fn record_download_failure_truncates_error_to_last_300_chars() {
        let (pool, video_id) = seed_single_video().await;
        let long_error = "x".repeat(500);

        record_download_failure(&pool, video_id, &long_error)
            .await
            .unwrap();

        let last_error: Option<String> =
            sqlx::query_scalar("SELECT last_download_error FROM videos WHERE id = ?")
                .bind(video_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            last_error.unwrap().len(),
            300,
            "last_download_error must be truncated to the last 300 chars"
        );
    }
}
