//! yt-dlp + FFmpeg binary management — download and verify tool availability.

use std::path::{Path, PathBuf};

/// Resolved paths to required external tools.
#[derive(Debug, Clone)]
pub struct ToolPaths {
    pub ytdlp: PathBuf,
    pub ffmpeg: PathBuf,
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

        Ok(ToolPaths { ytdlp, ffmpeg })
    }

    /// Run `yt-dlp --update` to get the latest version.
    pub async fn update_ytdlp(&self) -> Result<(), anyhow::Error> {
        let ytdlp = self.tools_dir.join(ytdlp_filename());
        anyhow::ensure!(ytdlp.exists(), "yt-dlp not found at {}", ytdlp.display());

        let output = tokio::process::Command::new(&ytdlp)
            .arg("--update")
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("yt-dlp --update failed: {stderr}");
        }

        tracing::info!("yt-dlp updated successfully");
        Ok(())
    }

    /// Get the yt-dlp version string.
    pub async fn ytdlp_version(&self, ytdlp: &Path) -> Result<String, anyhow::Error> {
        let output = tokio::process::Command::new(ytdlp)
            .arg("--version")
            .output()
            .await?;

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
}
