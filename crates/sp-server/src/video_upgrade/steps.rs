//! #223 S11: the production [`Steps`]: the box's yt-dlp, Media Foundation
//! (the reader that plays the video) and Symphonia (the audio's).
//!
//! Glue only, out of the mutation gate by `mutants::skip`: the decisions
//! are `video_upgrade`'s pure functions, Linux-tested with scripted steps.

use std::path::{Path, PathBuf};
use std::time::Duration;

use sp_decoder::{DecodeMode, MediaStream};

use super::{Steps, VideoFacts};
use crate::downloader::format::{self, DownloadedFormat};
use crate::downloader::tools::ToolPaths;
use crate::downloader::{probe, ytdlp_cmd, ytdlp_lock, ytdlp_video_args};

/// `CreateProcess` flags of the download: no console window, below normal.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;

/// The longest an upgrade's download may take before it is killed.
const DOWNLOAD_BOUND: Duration = Duration::from_secs(1800); // 30 min
/// The most of yt-dlp's error text an answer carries.
const ERROR_MAX_CHARS: usize = 400;

/// The box's tools, its cookie file and the playback decode mode.
pub(crate) struct Real {
    pub ytdlp: PathBuf,
    pub ffmpeg_dir: PathBuf,
    pub cookies: Option<PathBuf>,
    pub mode: DecodeMode,
}

impl Real {
    /// The box's steps: its tools, the cookie file in the data dir
    /// (`cache_dir`'s parent, as the probe and the download find it) when
    /// present, and the mode playback decodes in now.
    pub(crate) fn new(tools: &ToolPaths, cache_dir: &Path) -> Self {
        let cookies = cache_dir.parent().unwrap_or(cache_dir).join("cookies.txt");
        Self {
            ytdlp: tools.ytdlp.clone(),
            ffmpeg_dir: tools
                .ffmpeg
                .parent()
                .unwrap_or(Path::new("."))
                .to_path_buf(),
            cookies: cookies.exists().then_some(cookies),
            mode: crate::playback::video_decode::global().mode(),
        }
    }
}

impl Steps for Real {
    #[cfg_attr(test, mutants::skip)] // spawns yt-dlp (`probe::resolve`)
    async fn resolve(&self, youtube_id: &str, cap: u32) -> Result<DownloadedFormat, String> {
        let report = probe::resolve(&self.ytdlp, self.cookies.as_deref(), youtube_id, cap).await;
        report
            .format
            .ok_or_else(|| report.error.unwrap_or_else(|| "no format".to_string()))
    }

    #[cfg_attr(test, mutants::skip)] // the real reader on a decode thread
    async fn facts(&self, path: &Path) -> Result<VideoFacts, String> {
        facts_on_decode_thread(path.to_path_buf(), self.mode).await
    }

    #[cfg_attr(test, mutants::skip)] // spawns yt-dlp
    async fn download(
        &self,
        youtube_id: &str,
        format_id: &str,
        out: &Path,
    ) -> Result<Option<DownloadedFormat>, String> {
        // The self-update never swaps the binary while this runs.
        let lock = ytdlp_lock();
        let _guard = lock.lock().await;
        let url = format!("https://www.youtube.com/watch?v={youtube_id}");
        let args = ytdlp_video_args(
            format_id,
            &self.ffmpeg_dir,
            out,
            &url,
            self.cookies.as_deref(),
        );
        let mut cmd = ytdlp_cmd::ytdlp_command(&self.ytdlp);
        // #223 S12a: below playback; its ffmpeg child inherits the class.
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
        cmd.args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let output = match tokio::time::timeout(DOWNLOAD_BOUND, cmd.output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => return Err(format!("yt-dlp did not start: {e}")),
            Err(_) => {
                return Err(format!(
                    "yt-dlp did not finish in {} min",
                    DOWNLOAD_BOUND.as_secs() / 60
                ));
            }
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let last = stderr
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("no message");
            return Err(format!(
                "yt-dlp exited with {}: {}",
                output.status,
                last.chars().take(ERROR_MAX_CHARS).collect::<String>()
            ));
        }
        Ok(format::parse_downloaded_format(&String::from_utf8_lossy(
            &output.stdout,
        )))
    }

    #[cfg_attr(test, mutants::skip)] // the real audio reader
    async fn audio_ms(&self, path: &Path) -> Result<u64, String> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            sp_decoder::SymphoniaAudioReader::open(&path)
                .map(|reader| reader.duration_ms())
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| format!("the audio read did not finish: {e}"))?
    }
}

/// [`super::facts_of`] through Media Foundation, opened in `mode` on a
/// decode thread (COM's STA rule: the thread that opens a reader decodes
/// and drops it).
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)] // the real reader; the facts are `facts_of`, tested
async fn facts_on_decode_thread(path: PathBuf, mode: DecodeMode) -> Result<VideoFacts, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    crate::playback::decode_thread::spawn_decode_thread("video-upgrade".to_string(), move || {
        let facts = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut reader = sp_decoder::MediaFoundationVideoReader::open_with(&path, mode)
                .map_err(|e| format!("the open: {e}"))?;
            super::facts_of(&mut reader)
        }))
        .unwrap_or_else(|_| Err("the reader panicked".to_string()));
        let _ = tx.send(facts);
    })
    .map_err(|e| format!("the decode thread did not start: {e}"))?;
    rx.await
        .map_err(|_| "the decode thread ended with no answer".to_string())?
}

/// Without Media Foundation there is no reader to read the video with.
#[cfg(not(windows))]
#[cfg_attr(test, mutants::skip)] // a constant refusal
async fn facts_on_decode_thread(_path: PathBuf, _mode: DecodeMode) -> Result<VideoFacts, String> {
    Err("the video reader is Windows only".to_string())
}
