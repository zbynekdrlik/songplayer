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
        self.ensure_tools_with_ffmpeg_path_override(None).await
    }

    /// Same as `ensure_tools`, but lets a test point the non-Windows ffmpeg
    /// `PATH` probe at a controlled directory instead of inheriting the real
    /// environment — this is what makes both PATH-probe outcomes (found /
    /// not found) deterministic in tests regardless of whether the host
    /// running them happens to have ffmpeg installed.
    async fn ensure_tools_with_ffmpeg_path_override(
        &self,
        ffmpeg_path_override: Option<&str>,
    ) -> Result<ToolPaths, anyhow::Error> {
        // Only consulted by the non-Windows branch below; referenced here
        // unconditionally (Option<&str> is Copy, so this doesn't consume
        // the binding) so a Windows build doesn't warn on an unused arg.
        let _ = ffmpeg_path_override;

        tokio::fs::create_dir_all(&self.tools_dir).await?;

        let ytdlp = self.tools_dir.join(ytdlp_filename());
        // Reassigned only by the non-Windows PATH-probe branch below, so on
        // Windows the `mut` is genuinely unused.
        #[cfg_attr(windows, allow(unused_mut))]
        let mut ffmpeg = self.tools_dir.join(ffmpeg_filename());

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
            #[cfg(windows)]
            {
                tracing::info!("downloading ffmpeg to {}", ffmpeg.display());
                // FFmpeg for Windows is distributed as a ZIP archive — download and
                // extract the ffmpeg.exe binary from it.
                let zip_path = self.tools_dir.join("ffmpeg.zip");
                Self::download_file(ffmpeg_download_url(), &zip_path).await?;
                Self::extract_ffmpeg_from_zip(&zip_path, &ffmpeg).await?;
                let _ = tokio::fs::remove_file(&zip_path).await;
            }
            #[cfg(not(windows))]
            {
                // FFmpeg for Linux is distributed as a .tar.xz archive. This
                // crate has no tar/xz decoder, so downloading it straight
                // onto the `ffmpeg` binary path would write a corrupt
                // "executable" that fails `verify_executable`'s ELF check
                // forever, re-downloading ~80MB on every start while every
                // FFmpeg call fails with ENOEXEC. Windows is the shipped
                // target (see project CLAUDE.md "Deployment target"); this
                // branch only runs on a Linux dev/CI box.
                //
                // Most Linux boxes already have a package-manager-installed
                // ffmpeg on PATH (yt_subs-only lyrics paths don't even need
                // it) — probe for that first, same shape as `detect_python`
                // below, before giving up.
                if let Some(existing) = Self::detect_ffmpeg(ffmpeg_path_override).await {
                    tracing::info!(
                        "using ffmpeg found on PATH: {} (skipping the unsupported \
                         managed .tar.xz download)",
                        existing.display()
                    );
                    ffmpeg = existing;
                } else {
                    anyhow::bail!(
                        "automatic FFmpeg download is Windows-only (the Linux release at {} \
                         is a .tar.xz archive and this crate has no xz decoder), and no \
                         `ffmpeg` was found on PATH either. Install ffmpeg via the system \
                         package manager, or extract it from that archive yourself, and \
                         place/symlink the binary at {}",
                        ffmpeg_download_url(),
                        ffmpeg.display()
                    );
                }
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
            buf == *b"MZ" // PE header
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
                    if let Ok(path) = which_on_path(candidate, None).await {
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

    /// Detect an existing `ffmpeg` on `PATH` (non-Windows only) — mirrors
    /// `detect_python`'s probe-then-resolve shape. Returns `None` when
    /// `ffmpeg` is not runnable via `PATH`, so the caller falls through to
    /// the Windows-only auto-download's explanatory bail.
    ///
    /// `path_override`, when set, replaces the probe subprocess's `PATH` env
    /// var instead of inheriting the real one — used by tests to make both
    /// outcomes (found / not found) deterministic.
    #[cfg(not(windows))]
    async fn detect_ffmpeg(path_override: Option<&str>) -> Option<PathBuf> {
        let mut cmd = tokio::process::Command::new("ffmpeg");
        cmd.arg("-version");
        if let Some(path) = path_override {
            cmd.env("PATH", path);
        }
        super::hide_console_window(&mut cmd);
        let output = cmd.output().await.ok()?;
        if !output.status.success() {
            return None;
        }
        if let Ok(path) = which_on_path("ffmpeg", path_override).await {
            tracing::info!("ffmpeg resolved to absolute path: {}", path.display());
            return Some(path);
        }
        // Fallback: just return the bare command name — a child process
        // that inherits PATH will still find it, same fallback
        // `detect_python` uses above.
        Some(PathBuf::from("ffmpeg"))
    }
}

/// Try to resolve a command name to an absolute path using the OS `where`/`which` command.
///
/// `path_override`, when set, replaces the child's `PATH` env var instead of
/// inheriting the real one — used by tests to point the lookup at a
/// controlled directory instead of the real environment.
async fn which_on_path(name: &str, path_override: Option<&str>) -> Result<PathBuf, anyhow::Error> {
    #[cfg(windows)]
    let locator = "where";
    #[cfg(not(windows))]
    let locator = "which";

    let mut cmd = tokio::process::Command::new(locator);
    cmd.arg(name);
    if let Some(path) = path_override {
        cmd.env("PATH", path);
    }
    let output = cmd.output().await?;

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

    /// Write an executable no-op script at `dir/ffmpeg` (Unix shebang script
    /// that exits 0) so a `Command::new("ffmpeg").arg("-version")` probe
    /// succeeds when `PATH` is overridden to point at `dir`.
    #[cfg(not(windows))]
    async fn write_fake_ffmpeg(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let fake_ffmpeg = dir.join("ffmpeg");
        tokio::fs::write(&fake_ffmpeg, "#!/bin/sh\nexit 0\n")
            .await
            .expect("write fake ffmpeg script");
        let mut perms = tokio::fs::metadata(&fake_ffmpeg)
            .await
            .expect("stat fake ffmpeg script")
            .permissions();
        perms.set_mode(0o755);
        tokio::fs::set_permissions(&fake_ffmpeg, perms)
            .await
            .expect("chmod fake ffmpeg script");
    }

    /// When `ffmpeg` IS found on `PATH`, the non-Windows branch of
    /// `ensure_tools` must use it instead of bailing — most Linux boxes
    /// (incl. GitHub-hosted Ubuntu runners, which ship ffmpeg pre-installed)
    /// already have a package-manager ffmpeg, so the previous unconditional
    /// bail silently started zero workers on a machine that had everything
    /// it needed. `PATH` is overridden to an isolated directory containing
    /// only the fake ffmpeg, so this is deterministic regardless of whether
    /// the real host also happens to have ffmpeg installed.
    #[cfg(not(windows))]
    #[tokio::test]
    async fn ensure_tools_uses_ffmpeg_found_on_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tools_dir = dir.path().to_path_buf();
        tokio::fs::write(tools_dir.join(ytdlp_filename()), b"fake-ytdlp")
            .await
            .expect("write fake ytdlp");

        let fake_path_dir = tempfile::tempdir().expect("fake PATH dir");
        write_fake_ffmpeg(fake_path_dir.path()).await;
        let path_override = fake_path_dir.path().to_str().expect("utf8 tempdir path");

        let mgr = ToolsManager::new(tools_dir.clone());
        let paths = mgr
            .ensure_tools_with_ffmpeg_path_override(Some(path_override))
            .await
            .expect("ffmpeg on PATH must be used instead of bailing");
        assert_eq!(
            paths.ffmpeg.file_name().and_then(|n| n.to_str()),
            Some("ffmpeg"),
            "ffmpeg path was: {:?}",
            paths.ffmpeg
        );
        assert!(
            !tools_dir.join(ffmpeg_filename()).exists(),
            "must not attempt the unsupported managed .tar.xz download when ffmpeg is on PATH"
        );
    }

    /// When `ffmpeg` is NOT found on `PATH` either, the non-Windows branch
    /// of `ensure_tools` must still fail loudly instead of downloading the
    /// `.tar.xz` FFmpeg release straight onto the `ffmpeg` binary path —
    /// this crate has no tar/xz decoder, so the resulting file can never
    /// pass `verify_executable`'s ELF check. `PATH` is overridden to an
    /// empty directory so this is deterministic even on a host (e.g. a
    /// GitHub-hosted Ubuntu runner) that has ffmpeg pre-installed for real.
    /// Pre-seed a fake yt-dlp so the earlier step in `ensure_tools` doesn't
    /// attempt a real network download; only the ffmpeg branch is exercised.
    #[cfg(not(windows))]
    #[tokio::test]
    async fn ensure_tools_bails_loudly_instead_of_shipping_corrupt_ffmpeg() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tools_dir = dir.path().to_path_buf();
        tokio::fs::write(tools_dir.join(ytdlp_filename()), b"fake-ytdlp")
            .await
            .expect("write fake ytdlp");

        let empty_path_dir = tempfile::tempdir().expect("empty PATH dir");
        let path_override = empty_path_dir.path().to_str().expect("utf8 tempdir path");

        let mgr = ToolsManager::new(tools_dir.clone());
        let err = mgr
            .ensure_tools_with_ffmpeg_path_override(Some(path_override))
            .await
            .expect_err(
                "non-Windows ffmpeg auto-download must fail when nothing is on PATH either",
            );
        let msg = err.to_string();
        assert!(msg.contains("Windows-only"), "message was: {msg}");
        assert!(msg.contains("ffmpeg"), "message was: {msg}");
        assert!(
            !tools_dir.join(ffmpeg_filename()).exists(),
            "must not leave a corrupt file at the ffmpeg binary path"
        );
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
