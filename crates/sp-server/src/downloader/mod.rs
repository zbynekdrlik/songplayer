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
pub mod format;
pub mod normalize;
pub mod tools;
pub mod ytdlp_cmd;

use crate::metadata::ProviderChain;
use crate::metadata::manual::download_title;
use sqlx::SqlitePool;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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

/// Force UTF-8 stdio on a yt-dlp (frozen-Python) child (#136 T4).
///
/// yt-dlp is a frozen-Python program; on Windows its `--dump-json` stdout is
/// encoded with the process ANSI codepage (cp1252 on win-resolume), not
/// UTF-8, so a title outside cp1252's range (e.g. "Vámonos") is mangled
/// before `String::from_utf8_lossy` ever sees it. Both variables are needed:
/// `PYTHONUTF8=1` enables Python's UTF-8 mode process-wide, and a redirected
/// pipe additionally honors `PYTHONIOENCODING=utf-8` for the stream
/// encoding. Same mechanism `lyrics::mtl_aligner` already uses (#137).
pub fn apply_utf8_env(cmd: &mut tokio::process::Command) {
    cmd.env("PYTHONUTF8", "1");
    cmd.env("PYTHONIOENCODING", "utf-8");
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
        "--socket-timeout".into(),
        DOWNLOAD_TIMEOUT.to_string().into(),
        "--remux-video".into(),
        "mp4".into(),
        "--no-part".into(),
        // #223 S9a: a leftover temp of a crashed run is overwritten, never
        // taken for the download.
        "--force-overwrites".into(),
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

/// Serializes every yt-dlp invocation against `yt-dlp --update` (#140 review
/// finding): on Windows the binary cannot be replaced while a download is
/// running it, so the self-updater and the download worker take this lock in
/// turn — an update waits for the in-flight song, a download waits for the
/// swap — instead of the update silently failing on a file lock.
pub type YtdlpLock = std::sync::Arc<tokio::sync::Mutex<()>>;

/// Background worker that downloads, extracts metadata, and normalizes videos.
pub struct DownloadWorker {
    pool: SqlitePool,
    tools: ToolPaths,
    cache_dir: PathBuf,
    ytdlp_lock: YtdlpLock,
    /// Directory holding the app's data (same directory as the SQLite DB).
    /// A `cookies.txt` Netscape cookie file dropped here — production path
    /// `C:\ProgramData\SongPlayer\cookies.txt` — is passed to yt-dlp on
    /// every download to work around YouTube's anonymous-download bot
    /// check (#141). Re-checked per download, not cached at startup, so a
    /// cookie file added later takes effect without a restart.
    data_dir: PathBuf,
    /// The ONE production metadata chain (#136), shared with the reprocess worker.
    metadata: Arc<ProviderChain>,
    event_tx: broadcast::Sender<String>,
    /// #229: this node in the exchange, asked before each download (`None`
    /// in tests that do not need it).
    peer: Option<Arc<crate::peer::Exchange>>,
}

impl DownloadWorker {
    pub fn new(
        pool: SqlitePool,
        tools: ToolPaths,
        cache_dir: PathBuf,
        data_dir: PathBuf,
        metadata: Arc<ProviderChain>,
        event_tx: broadcast::Sender<String>,
        ytdlp_lock: YtdlpLock,
    ) -> Self {
        Self {
            pool,
            tools,
            cache_dir,
            ytdlp_lock,
            data_dir,
            metadata,
            event_tx,
            peer: None,
        }
    }

    /// #229: ask the exchange's peers before each download.
    pub fn with_peer(mut self, peer: Arc<crate::peer::Exchange>) -> Self {
        self.peer = Some(peer);
        self
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
                _ = async {
                    // Hold the yt-dlp lock for the whole song so a
                    // `yt-dlp --update` never swaps the binary mid-download.
                    let _ytdlp_guard = self.ytdlp_lock.lock().await;
                    self.process_next().await
                } => {}
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
        // #230: a held background starts no new job (a running one finishes).
        if crate::background_hold::holds(&self.pool, crate::background_hold::Job::Download).await {
            return false;
        }
        let row = match fetch_next_unprocessed(&self.pool).await {
            Ok(Some(r)) => r,
            Ok(None) => return false,
            Err(e) => {
                tracing::error!("failed to fetch next video: {e}");
                return false;
            }
        };

        tracing::info!(video_id = %row.youtube_id, title = %row.title, "processing video");
        // #229: ask the peers first (`peer::download`): a peer's pair is
        // taken, a peer's download waited for; else run it here, announced
        // until this function returns.
        let _announced = match crate::peer::download::first(self.peer.as_ref(), &row).await {
            crate::peer::PeerStep::Done => {
                let _ = self.event_tx.send(format!("processed:{}", row.youtube_id));
                return true;
            }
            crate::peer::PeerStep::Deferred => return false,
            crate::peer::PeerStep::Local(guard) => guard,
        };
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
        // #223 S9a: a crashed run's temps never stay next to the song.
        cleanup_temps(&video_temp, &self.cache_dir, &row.youtube_id);

        if let Err(e) = self
            .download_video_stream(&row.youtube_id, &video_temp)
            .await
        {
            tracing::error!(video_id = %row.youtube_id, "video download failed: {e}");
            cleanup_temps(&video_temp, &self.cache_dir, &row.youtube_id);
            self.record_failure(row.id, &row.youtube_id, &e.to_string())
                .await;
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
                self.record_failure(row.id, &row.youtube_id, &e.to_string())
                    .await;
                return false;
            }
        };

        // #136: a corrected video keeps its operator's title.
        let meta = download_title(&self.pool, &self.metadata, &row.youtube_id, &row.title).await;

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
            self.record_failure(row.id, &row.youtube_id, &e.to_string())
                .await;
            return false;
        }

        // The audio temp is normalized into `audio_final`: no path needs it.
        let _ = tokio::fs::remove_file(&audio_temp).await;

        // Move the video temp to its final pair name.
        if let Err(e) = place_video(&self.pool, &video_temp, &video_final, &audio_final).await {
            tracing::error!(video_id = %row.youtube_id, "video rename failed: {e}");
            self.record_failure(row.id, &row.youtube_id, &e.to_string())
                .await;
            return false;
        }

        if let Err(e) = crate::metadata::manual::record_download(
            &self.pool,
            &self.cache_dir,
            row.id,
            &row.youtube_id,
            &meta,
            &video_final,
            &audio_final,
        )
        .await
        {
            tracing::error!(video_id = %row.youtube_id, "DB update failed: {e}");
            self.record_failure(row.id, &row.youtube_id, &e.to_string())
                .await;
            return false;
        }

        let _ = self.event_tx.send(format!("processed:{}", row.youtube_id));
        tracing::info!(video_id = %row.youtube_id, "video processed successfully");
        true
    }

    /// Record a download/normalize/DB failure for `video_id` (#140): bumps
    /// `download_attempts`, stores the error tail, and schedules
    /// `next_attempt_at` via exponential backoff — so one broken video
    /// backs off instead of blocking every video behind it in the queue
    /// forever. Swallows its own DB error (already in a failure path;
    /// nothing more useful to do than log it).
    async fn record_failure(&self, video_id: i64, youtube_id: &str, error: &str) {
        if let Err(e) = record_download_failure(&self.pool, video_id, error).await {
            tracing::error!(video_id = %youtube_id, "failed to record download failure: {e}");
        }
    }

    /// Download the video stream only via yt-dlp.
    async fn download_video_stream(
        &self,
        video_id: &str,
        output: &Path,
    ) -> Result<(), anyhow::Error> {
        let url = format!("https://www.youtube.com/watch?v={video_id}");
        // #223 S9a: the cap is read live at every download.
        let cap = format::max_resolution(
            crate::db::models::get_setting(&self.pool, sp_core::config::SETTING_MAX_RESOLUTION)
                .await
                .ok()
                .flatten()
                .as_deref(),
        );
        tracing::info!(video_id, cap, "download: video stream at most {cap} rows");
        let format_spec = format::format_spec(cap);
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

        // ytdlp_command puts the tools dir first on PATH (bundled deno) + the
        // `--js-runtimes deno` flag + CREATE_NO_WINDOW + UTF-8 env (#189).
        let mut cmd = ytdlp_cmd::ytdlp_command(&self.tools.ytdlp);
        cmd.args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
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

        // Same n-challenge-aware builder as the video stream (#189).
        let mut cmd = ytdlp_cmd::ytdlp_command(&self.tools.ytdlp);
        cmd.args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
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

/// The rows the download worker takes (#140): `normalized = 0` on an active
/// playlist whose `next_attempt_at` is NULL (never failed) or already due
/// (`v.` / `p.` aliases). Binds ONE `?`: now, `chrono::Utc::now().to_rfc3339()`.
/// The node exchange lists these rows as queued downloads (#229, `peer::queued`).
pub(crate) const DOWNLOAD_DUE: &str = concat!(
    "v.normalized = 0 AND p.is_active = 1 \
     AND (v.next_attempt_at IS NULL OR v.next_attempt_at <= ?) AND ",
    crate::test_item::not_test_item!() // #228: the test item is imported, never downloaded
);

/// Fetch the next video that needs processing ([`DOWNLOAD_DUE`]). Rows with
/// a NULL `next_attempt_at` sort before due-now rows so a fresh video is never
/// starved behind a backlog of retries, and within each group the lowest `id`
/// wins — same FIFO order as before #140 for the common (no-failure) case.
pub(crate) async fn fetch_next_unprocessed(
    pool: &SqlitePool,
) -> Result<Option<VideoRow>, sqlx::Error> {
    let now = chrono::Utc::now().to_rfc3339();
    let row = sqlx::query_as::<_, VideoRow>(&format!(
        "SELECT v.id, v.youtube_id, COALESCE(v.title, '') as title
         FROM videos v
         JOIN playlists p ON p.id = v.playlist_id
         WHERE {DOWNLOAD_DUE}
         ORDER BY (v.next_attempt_at IS NOT NULL), v.id
         LIMIT 1",
    ))
    .bind(&now)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Exponential retry backoff (#140): `5 min * 2^(attempts-1)`, capped at
/// 24h. `attempts == 0` (defensive — callers always pass `>= 1`) is treated
/// the same as `attempts == 1`. Uses checked/saturating math throughout so
/// no `attempts` value (including `u32::MAX`) can panic on overflow — it
/// just saturates at the 24h cap.
pub(crate) fn retry_backoff(attempts: u32) -> std::time::Duration {
    const BASE_SECS: u64 = 5 * 60;
    const CAP_SECS: u64 = 24 * 60 * 60;

    let exponent = attempts.saturating_sub(1).min(63);
    let multiplier = 1u64.checked_shl(exponent).unwrap_or(u64::MAX);
    let secs = BASE_SECS.saturating_mul(multiplier);
    std::time::Duration::from_secs(secs.min(CAP_SECS))
}

/// Record a download/normalize/DB-write failure for `video_id` (#140):
/// increments `download_attempts`, stores the last 300 chars of `error`,
/// and schedules `next_attempt_at` via [`retry_backoff`]. Returns the new
/// attempt count. Logs a `warn!` on every call, plus an additional
/// `error!` once attempts reach 5 (a video that has failed 5 times in a
/// row is worth operator attention).
pub(crate) async fn record_download_failure(
    pool: &SqlitePool,
    video_id: i64,
    error: &str,
) -> Result<i64, sqlx::Error> {
    let current: i64 = sqlx::query_scalar("SELECT download_attempts FROM videos WHERE id = ?")
        .bind(video_id)
        .fetch_one(pool)
        .await?;
    let new_attempts = current + 1;
    let backoff = retry_backoff(new_attempts as u32);
    let next_attempt_at = (chrono::Utc::now()
        + chrono::Duration::from_std(backoff).unwrap_or_else(|_| chrono::Duration::zero()))
    .to_rfc3339();
    let error_tail = crate::playlist::tail(error, 300);

    sqlx::query(
        "UPDATE videos
         SET download_attempts = ?, last_download_error = ?, next_attempt_at = ?
         WHERE id = ?",
    )
    .bind(new_attempts)
    .bind(error_tail)
    .bind(&next_attempt_at)
    .bind(video_id)
    .execute(pool)
    .await?;

    tracing::warn!(
        video_id,
        attempts = new_attempts,
        next_attempt_at = %next_attempt_at,
        "download failed, scheduled retry"
    );
    if new_attempts >= 5 {
        tracing::error!(
            video_id,
            attempts = new_attempts,
            "download has failed repeatedly — needs operator attention"
        );
    }

    Ok(new_attempts)
}

/// The fresh video temp → its final pair name. When it cannot take that name,
/// this attempt's files go: the video temp, and the normalized audio unless a
/// row records it. Rows of one video share files by name (#136), so
/// `audio_final` can be the audio another row of the video plays (its video
/// held open there is what fails this rename on Windows): checked and deleted
/// under `cache::SONG_FILES`, kept when the rows cannot be read.
async fn place_video(
    pool: &SqlitePool,
    video_temp: &Path,
    video_final: &Path,
    audio_final: &Path,
) -> std::io::Result<()> {
    let Err(e) = tokio::fs::rename(video_temp, video_final).await else {
        return Ok(());
    };
    let _ = tokio::fs::remove_file(video_temp).await;
    let _files = cache::SONG_FILES.lock().await;
    match cache::recorded_by_a_row(pool, audio_final).await {
        Ok(false) => {
            let _ = tokio::fs::remove_file(audio_final).await;
        }
        Ok(true) => tracing::warn!(
            audio = %audio_final.display(),
            "download: the video could not take its name - keeping the audio a row records"
        ),
        Err(db) => tracing::warn!(
            audio = %audio_final.display(),
            "download: reading which rows record the audio failed - keeping it: {db}"
        ),
    }
    Err(e)
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
pub(crate) struct VideoRow {
    pub(crate) id: i64,
    pub(crate) youtube_id: String,
    pub(crate) title: String,
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "mod_tests_peer.rs"]
mod tests_peer;
