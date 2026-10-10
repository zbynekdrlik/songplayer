//! #232: the YouTube probe behind the live gate `e2e/post-deploy-youtube.spec.ts`.
//!
//! The only YouTube check before it was the deno self-check
//! (`ytdlp_cmd::run_selfcheck`), which passes on a bot check, so expired
//! cookies, a YouTube change yt-dlp cannot extract, or a selector that
//! finds nothing would fail every download with CI green.
//!
//! The probe resolves ONE fixed real video ([`PROBE_VIDEO`]) exactly as a
//! download does: the box's yt-dlp through `ytdlp_cmd::ytdlp_command`, the
//! production selector (`format::format_spec`) at the live
//! `max_resolution`, and the cookie file when one is present. It prints the
//! format it would fetch ([`format::FORMAT_PROBE_PRINT`], yt-dlp's video
//! stage, which only simulates) and downloads nothing. No yt-dlp lock:
//! playlist sync, captions and the import already run yt-dlp beside a
//! download; only the binary's self-update needs the lock, and it ends
//! seconds after the start.

use std::ffi::OsString;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::Serialize;

use super::format::{self, DownloadedFormat};

/// The video the probe resolves: the metadata gate's (`post-deploy-metadata`),
/// a real catalog song with 1080p on YouTube (10.10.2026: AV1 1920×1080).
pub(crate) const PROBE_VIDEO: &str = "gq-4FVRr_ow";

/// The bound of one probe: yt-dlp answered in 2.6 s on SNV (10.10.2026).
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(120);

/// The longest error text the report carries.
const ERROR_MAX_CHARS: usize = 400;

/// `POST /api/v1/youtube/probe`'s answer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct YoutubeProbeReport {
    /// A format was resolved.
    pub ok: bool,
    pub youtube_id: String,
    /// The `max_resolution` cap the selector was built for.
    pub cap: u32,
    /// The cookie file was passed (#141: without it YouTube asks to sign in).
    pub cookies: bool,
    /// The format a download would fetch.
    pub format: Option<DownloadedFormat>,
    /// Why no format was resolved: yt-dlp's last `ERROR:` line, or what
    /// stopped the probe.
    pub error: Option<String>,
    pub elapsed_ms: u64,
}

/// yt-dlp's arguments for the probe (the URL last; `ytdlp_command` adds the
/// JS runtime).
pub(crate) fn probe_args(spec: &str, cookies: Option<&Path>, url: &str) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "-f".into(),
        spec.into(),
        "--socket-timeout".into(),
        "30".into(),
        "--print".into(),
        format::FORMAT_PROBE_PRINT.into(),
    ];
    if let Some(cookies) = cookies {
        args.push("--cookies".into());
        args.push(cookies.into());
    }
    args.push(url.into());
    args
}

/// The report of a probe whose yt-dlp exited (`exit_ok`) with `stdout` and
/// `stderr`.
pub(crate) fn report(
    youtube_id: &str,
    cap: u32,
    cookies: bool,
    exit_ok: bool,
    stdout: &str,
    stderr: &str,
    elapsed_ms: u64,
) -> YoutubeProbeReport {
    let format = format::parse_downloaded_format(stdout);
    let error = if !exit_ok {
        Some(yt_dlp_error(stderr))
    } else if format.is_none() {
        Some("yt-dlp printed no format line".to_string())
    } else {
        None
    };
    YoutubeProbeReport {
        ok: error.is_none(),
        youtube_id: youtube_id.to_string(),
        cap,
        cookies,
        format,
        error,
        elapsed_ms,
    }
}

/// A report that never reached yt-dlp's answer (not ready, spawn failed,
/// timed out).
pub(crate) fn refused(
    youtube_id: &str,
    cap: u32,
    cookies: bool,
    why: String,
) -> YoutubeProbeReport {
    YoutubeProbeReport {
        ok: false,
        youtube_id: youtube_id.to_string(),
        cap,
        cookies,
        format: None,
        error: Some(why),
        elapsed_ms: 0,
    }
}

/// yt-dlp's last `ERROR:` line (the cause: a bot check, a sign-in, an
/// extraction failure), else its last non-empty line, cut to
/// [`ERROR_MAX_CHARS`].
fn yt_dlp_error(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let line = lines
        .iter()
        .rev()
        .find(|l| l.starts_with("ERROR:"))
        .or(lines.last())
        .copied()
        .unwrap_or("yt-dlp failed with no message");
    line.chars().take(ERROR_MAX_CHARS).collect()
}

/// Run the probe with the box's yt-dlp (module doc).
#[cfg_attr(test, mutants::skip)] // one call; `resolve` is the spawn
pub(crate) async fn run(ytdlp: &Path, cookies: Option<&Path>, cap: u32) -> YoutubeProbeReport {
    resolve(ytdlp, cookies, PROBE_VIDEO, cap).await
}

/// What a download of `youtube_id` at `cap` would fetch, downloading
/// nothing (the probe's video, or a song the video upgrade checks, #223
/// S11).
#[cfg_attr(test, mutants::skip)] // spawns yt-dlp; `probe_args` and `report` are tested
pub(crate) async fn resolve(
    ytdlp: &Path,
    cookies: Option<&Path>,
    youtube_id: &str,
    cap: u32,
) -> YoutubeProbeReport {
    let url = format!("https://www.youtube.com/watch?v={youtube_id}");
    let mut cmd = super::ytdlp_cmd::ytdlp_command(ytdlp);
    cmd.args(probe_args(&format::format_spec(cap), cookies, &url))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let started = Instant::now();
    let has_cookies = cookies.is_some();
    match tokio::time::timeout(PROBE_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) => report(
            youtube_id,
            cap,
            has_cookies,
            out.status.success(),
            &String::from_utf8_lossy(&out.stdout),
            &String::from_utf8_lossy(&out.stderr),
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        ),
        Ok(Err(e)) => refused(
            youtube_id,
            cap,
            has_cookies,
            format!("yt-dlp did not start: {e}"),
        ),
        Err(_) => refused(
            youtube_id,
            cap,
            has_cookies,
            format!("yt-dlp did not answer in {} s", PROBE_TIMEOUT.as_secs()),
        ),
    }
}

#[cfg(test)]
#[path = "probe_tests.rs"]
mod tests;
