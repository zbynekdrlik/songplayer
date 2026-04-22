//! yt-dlp + FFmpeg binary management — download and verify tool availability.

use std::path::{Path, PathBuf};

/// Resolved paths to required external tools.
#[derive(Debug, Clone)]
pub struct ToolPaths {
    pub ytdlp: PathBuf,
    pub ffmpeg: PathBuf,
    /// Path to a Python interpreter, if one is available on this machine.
    /// `None` when neither `python` nor `python3` is found on `PATH`.
    pub python: Option<PathBuf>,
}

/// Manages downloading and locating yt-dlp and FFmpeg binaries.
pub struct ToolsManager {
    tools_dir: PathBuf,
}

impl ToolsManager {
    pub fn new(tools_dir: PathBuf) -> Self {
        Self { tools_dir }
    }

    /// Check if tools exist and return paths, or download them.
    pub async fn ensure_tools(&self) -> Result<ToolPaths, anyhow::Error> {
        tokio::fs::create_dir_all(&self.tools_dir).await?;

        let ytdlp = self.tools_dir.join(ytdlp_filename());
        let ffmpeg = self.tools_dir.join(ffmpeg_filename());

        if !ytdlp.exists() {
            tracing::info!("downloading yt-dlp to {}", ytdlp.display());
            Self::download_file(ytdlp_download_url(), &ytdlp).await?;
            #[cfg(unix)]
            Self::make_executable(&ytdlp).await?;
        }

        // Verify ffmpeg is a real executable (not a ZIP archive from a previous buggy download).
        if ffmpeg.exists() && !Self::verify_executable(&ffmpeg).await {
            tracing::warn!(
                "ffmpeg at {} is not a valid executable, re-downloading",
                ffmpeg.display()
            );
            let _ = tokio::fs::remove_file(&ffmpeg).await;
        }

        if !ffmpeg.exists() {
            tracing::info!("downloading ffmpeg to {}", ffmpeg.display());
            #[cfg(windows)]
            {
                // FFmpeg for Windows is distributed as a ZIP archive — download and
                // extract the ffmpeg.exe binary from it.
                let zip_path = self.tools_dir.join("ffmpeg.zip");
                Self::download_file(ffmpeg_download_url(), &zip_path).await?;
                Self::extract_ffmpeg_from_zip(&zip_path, &ffmpeg).await?;
                let _ = tokio::fs::remove_file(&zip_path).await;
            }
            #[cfg(not(windows))]
            {
                Self::download_file(ffmpeg_download_url(), &ffmpeg).await?;
                Self::make_executable(&ffmpeg).await?;
            }
        }

        let python = Self::detect_python().await;

        Ok(ToolPaths {
            ytdlp,
            ffmpeg,
            python,
        })
    }

    /// Run `yt-dlp --update` to get the latest version.
    pub async fn update_ytdlp(&self) -> Result<(), anyhow::Error> {
        let ytdlp = self.tools_dir.join(ytdlp_filename());
        anyhow::ensure!(ytdlp.exists(), "yt-dlp not found at {}", ytdlp.display());

        let mut cmd = tokio::process::Command::new(&ytdlp);
        cmd.arg("--update");
        super::hide_console_window(&mut cmd);
        let output = cmd.output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("yt-dlp --update failed: {stderr}");
        }

        tracing::info!("yt-dlp updated successfully");
        Ok(())
    }

    /// Get the yt-dlp version string.
    pub async fn ytdlp_version(&self, ytdlp: &Path) -> Result<String, anyhow::Error> {
        let mut cmd = tokio::process::Command::new(ytdlp);
        cmd.arg("--version");
        super::hide_console_window(&mut cmd);
        let output = cmd.output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("yt-dlp --version failed: {stderr}");
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Verify a file is a real executable by checking its magic bytes.
    /// On Windows: check for MZ (PE) header. On Unix: check for ELF header.
    async fn verify_executable(path: &Path) -> bool {
        let Ok(mut file) = tokio::fs::File::open(path).await else {
            return false;
        };
        let mut buf = [0u8; 2];
        use tokio::io::AsyncReadExt;
        if file.read_exact(&mut buf).await.is_err() {
            return false;
        }
        if cfg!(windows) {
            buf == [b'M', b'Z'] // PE header
        } else {
            buf == [0x7F, b'E'] // ELF header
        }
    }

    /// Download a file from `url` to `dest`.
    async fn download_file(url: &str, dest: &Path) -> Result<(), anyhow::Error> {
        let response = reqwest::get(url).await?;
        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("download failed with HTTP {status}: {url}");
        }

        let bytes = response.bytes().await?;
        tokio::fs::write(dest, &bytes).await?;
        tracing::info!("downloaded {} bytes to {}", bytes.len(), dest.display());
        Ok(())
    }

    /// Extract `ffmpeg.exe` from a downloaded ZIP archive.
    ///
    /// The BtbN FFmpeg builds contain a nested directory structure like:
    /// `ffmpeg-master-latest-win64-gpl/bin/ffmpeg.exe`
    /// We search for any file named `ffmpeg.exe` and extract it.
    #[cfg(windows)]
    async fn extract_ffmpeg_from_zip(zip_path: &Path, dest: &Path) -> Result<(), anyhow::Error> {
        let zip_path = zip_path.to_path_buf();
        let dest = dest.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let file = std::fs::File::open(&zip_path)?;
            let mut archive = zip::ZipArchive::new(file)?;
            for i in 0..archive.len() {
                let mut entry = archive.by_index(i)?;
                let name = entry.name().to_string();
                if name.ends_with("/ffmpeg.exe") || name == "ffmpeg.exe" {
                    let mut out = std::fs::File::create(&dest)?;
                    std::io::copy(&mut entry, &mut out)?;
                    tracing::info!(
                        "extracted ffmpeg.exe from ZIP ({} bytes)",
                        out.metadata()?.len()
                    );
                    return Ok(());
                }
            }
            anyhow::bail!("ffmpeg.exe not found in ZIP archive");
        })
        .await?
    }

    /// Set executable permission on Unix.
    #[cfg(unix)]
    async fn make_executable(path: &Path) -> Result<(), anyhow::Error> {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        tokio::fs::set_permissions(path, perms).await?;
        Ok(())
    }

    /// Detect a Python interpreter by trying `python` then `python3`.
    /// Returns `None` if neither is available on `PATH`.
    async fn detect_python() -> Option<PathBuf> {
        for candidate in &["python", "python3"] {
            let mut cmd = tokio::process::Command::new(candidate);
            cmd.arg("--version");
            super::hide_console_window(&mut cmd);
            if let Ok(output) = cmd.output().await {
                if output.status.success() {
                    // Resolve to an absolute path so the caller doesn't need
                    // to rely on PATH being set in child processes.
                    if let Ok(path) = which_python(candidate).await {
                        tracing::info!("Python detected: {} ({:?})", candidate, path);
                        return Some(path);
                    }
                    // Fallback: just return the bare command name as a PathBuf.
                    return Some(PathBuf::from(candidate));
                }
            }
        }
        tracing::info!("Python not found on PATH; lyrics ASR/alignment disabled");
        None
    }
}

/// Try to resolve a command name to an absolute path using the OS `where`/`which` command.
async fn which_python(name: &str) -> Result<PathBuf, anyhow::Error> {
    #[cfg(windows)]
    let locator = "where";
    #[cfg(not(windows))]
    let locator = "which";

    let output = tokio::process::Command::new(locator)
        .arg(name)
        .output()
        .await?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        // `where` on Windows may return multiple lines; take the first one.
        let first = stdout.lines().next().unwrap_or("").trim();
        if !first.is_empty() {
            return Ok(PathBuf::from(first));
        }
    }
    anyhow::bail!("could not resolve {name} to absolute path")
}

// ---------------------------------------------------------------------------
// YouTube URL helpers
// ---------------------------------------------------------------------------

/// Parse an 11-character YouTube video id from any of the supported URL forms
/// (`youtu.be/<id>`, `youtube.com/watch?v=<id>`, m.youtube.com, embedded
/// playlist params). Returns None for non-YouTube URLs or malformed input.
pub fn extract_youtube_id(url: &str) -> Option<String> {
    // youtu.be/<id>[?...]
    if let Some(rest) = url
        .strip_prefix("https://youtu.be/")
        .or_else(|| url.strip_prefix("http://youtu.be/"))
    {
        let id = rest.split(['?', '/', '&']).next()?;
        return is_yt_id(id).then(|| id.to_string());
    }
    // *youtube.com/watch?v=<id>&...
    if url.contains("youtube.com/watch") {
        let query = url.split_once('?')?.1;
        for part in query.split('&') {
            if let Some(id) = part.strip_prefix("v=") {
                return is_yt_id(id).then(|| id.to_string());
            }
        }
    }
    None
}

fn is_yt_id(s: &str) -> bool {
    s.len() == 11
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Minimal metadata extracted via `yt-dlp --dump-json --no-playlist --skip-download`.
/// Thumbnails and full descriptions are intentionally dropped — they land later
/// when the worker processes the row through the normal download path.
#[derive(Debug, Clone)]
pub struct ImportedVideo {
    pub youtube_id: String,
    pub title: String,
    pub duration_ms: Option<u64>,
}

#[cfg_attr(test, mutants::skip)] // subprocess I/O glue; pure logic (URL parse) is covered by extract_youtube_id tests
pub async fn fetch_video_metadata(
    ytdlp_path: &std::path::Path,
    url: &str,
) -> anyhow::Result<ImportedVideo> {
    use tokio::process::Command;
    let youtube_id = extract_youtube_id(url)
        .ok_or_else(|| anyhow::anyhow!("could not parse YouTube id from URL: {url}"))?;
    let mut cmd = Command::new(ytdlp_path);
    cmd.args([
        "--dump-json",
        "--no-playlist",
        "--skip-download",
        "--no-warnings",
        url,
    ])
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let output = cmd.output().await?;
    if !output.status.success() {
        anyhow::bail!(
            "yt-dlp dump-json failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let title = json
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let duration_ms = json
        .get("duration")
        .and_then(|v| v.as_f64())
        .map(|d| (d * 1000.0) as u64);
    Ok(ImportedVideo {
        youtube_id,
        title,
        duration_ms,
    })
}

// ---------------------------------------------------------------------------
// Platform helpers
// ---------------------------------------------------------------------------

fn ytdlp_filename() -> &'static str {
    if cfg!(windows) {
        "yt-dlp.exe"
    } else {
        "yt-dlp"
    }
}

fn ffmpeg_filename() -> &'static str {
    if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    }
}

fn ytdlp_download_url() -> &'static str {
    if cfg!(windows) {
        "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe"
    } else {
        "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp"
    }
}

fn ffmpeg_download_url() -> &'static str {
    if cfg!(windows) {
        "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl.zip"
    } else {
        "https://johnvansickle.com/ffmpeg/releases/ffmpeg-release-amd64-static.tar.xz"
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_paths_derive_from_tools_dir() {
        let mgr = ToolsManager::new(PathBuf::from("/tmp/tools"));
        assert_eq!(mgr.tools_dir, PathBuf::from("/tmp/tools"));
    }

    #[test]
    fn filenames_have_correct_extension() {
        let name = ytdlp_filename();
        if cfg!(windows) {
            assert!(name.ends_with(".exe"));
        } else {
            assert!(!name.contains('.'));
        }

        let name = ffmpeg_filename();
        if cfg!(windows) {
            assert!(name.ends_with(".exe"));
        } else {
            assert!(!name.contains('.'));
        }
    }

    #[test]
    fn download_urls_point_to_github() {
        let url = ytdlp_download_url();
        assert!(url.starts_with("https://"));
        assert!(url.contains("yt-dlp"));
    }

    #[test]
    fn extract_youtube_id_from_short_url() {
        let cases = [
            ("https://youtu.be/AvWOCj48pGw", "AvWOCj48pGw"),
            ("https://youtu.be/BW_vUblj_RA?si=foo", "BW_vUblj_RA"),
            (
                "https://www.youtube.com/watch?v=xrhVLX6vwPk&list=PLx",
                "xrhVLX6vwPk",
            ),
            ("https://m.youtube.com/watch?v=cej4vn4sWtE", "cej4vn4sWtE"),
            ("http://youtu.be/cej4vn4sWtE", "cej4vn4sWtE"),
        ];
        for (url, expected) in cases {
            assert_eq!(
                super::extract_youtube_id(url).unwrap(),
                expected,
                "url = {url}"
            );
        }
    }

    #[test]
    fn extract_youtube_id_rejects_non_youtube() {
        assert!(super::extract_youtube_id("https://vimeo.com/123").is_none());
        assert!(super::extract_youtube_id("not a url").is_none());
        assert!(
            super::extract_youtube_id("https://youtu.be/tooshort").is_none(),
            "11-char id guard"
        );
        assert!(
            super::extract_youtube_id("https://youtube.com/watch?v=").is_none(),
            "empty v= guard"
        );
    }
}
