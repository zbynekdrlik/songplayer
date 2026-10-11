//! #223 S11: a cached song's video upgraded in place (design comment
//! 6103060545, revision 2's D9). Only `{base}_video.mp4` is replaced, under
//! its own name: the audio, stems, dub, lyrics and every row's paths stay as
//! they are, and `normalized` is never reset.
//!
//! One song, in order ([`run`]):
//!
//! 1. what a download at the live cap would fetch now (yt-dlp at the video
//!    stage, nothing downloaded);
//! 2. the cached video's facts through the reader that plays it;
//! 3. nothing taller: `no_better`, nothing downloaded ([`better`]);
//! 4. that exact stream downloaded into `{id}_video_upgrade_temp.mp4`;
//! 5. checked ([`verify`]): the rows asked, taller than the cached one, the
//!    audio's length, a picture near the end decodes, the first picture where
//!    the old one's was (lyrics and the title follow the audio clock);
//! 6. swapped in under the song-file lock, the old file kept as
//!    `<name>.prev` ([`swap::swap`]);
//! 7. recorded on every row of the video: V34's format and V35's check.
//!
//! The steps that run yt-dlp and the readers are a [`Steps`]: production's
//! is `steps::Real`, the tests script one. S12 adds the worker that runs
//! this by itself, the `.prev` retention and the rollback.

use std::path::{Path, PathBuf};

use serde::Serialize;
use sp_decoder::VideoStream;
use sqlx::SqlitePool;

use crate::downloader::format::{self, DownloadedFormat};

pub(crate) mod disk;
pub(crate) mod steps;
pub(crate) mod swap;
#[cfg(test)]
mod test_rig;
pub(crate) mod worker;

/// Rows Media Foundation may pad a picture's height by (its 16-row blocks: a
/// 1080-row stream can read 1088).
pub(crate) const ROW_PAD: u32 = 16;
/// How far the new video's length may be from the audio's.
pub(crate) const DURATION_SLACK_MS: u64 = 1000;
/// The near-end picture is read this long before the end.
pub(crate) const NEAR_END_MS: u64 = 2000;

/// What the reader that plays a video file reads of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct VideoFacts {
    pub width: u32,
    pub height: u32,
    /// The first picture's time.
    pub first_ms: u64,
    pub duration_ms: u64,
    /// One frame period in µs (0 = the reader knows no rate).
    pub frame_us: u64,
    /// A picture [`NEAR_END_MS`] before the end decoded.
    pub end_decoded: bool,
}

/// The facts of an opened video: its first picture, its rate and length,
/// then one picture [`NEAR_END_MS`] before the end. Its one production
/// caller is the Windows reader's (`steps.rs`).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn facts_of<V: VideoStream + ?Sized>(video: &mut V) -> Result<VideoFacts, String> {
    let first = video
        .next_frame()
        .map_err(|e| format!("the first picture: {e}"))?
        .ok_or("no picture")?;
    let duration_ms = video.duration_ms();
    let (num, den) = video.frame_rate();
    let frame_us = if num == 0 {
        0
    } else {
        u64::from(den) * 1_000_000 / u64::from(num)
    };
    let near_end = duration_ms.saturating_sub(NEAR_END_MS);
    video
        .seek(near_end)
        .map_err(|e| format!("the seek to {near_end} ms: {e}"))?;
    let end_decoded = matches!(video.next_frame(), Ok(Some(_)));
    Ok(VideoFacts {
        width: first.width,
        height: first.height,
        first_ms: first.timestamp_ms,
        duration_ms,
        frame_us,
        end_decoded,
    })
}

/// Whether `resolved` has more rows than the cached video. The cached
/// height may carry the reader's padding, so a stream within it is no
/// upgrade.
pub(crate) fn better(resolved: &DownloadedFormat, old: &VideoFacts) -> bool {
    resolved.height.is_some_and(|height| height > old.height)
}

/// Whether the downloaded video may replace the cached one ([`run`] step 5),
/// else why not.
pub(crate) fn verify(
    resolved_height: u32,
    old: &VideoFacts,
    new: &VideoFacts,
    audio_ms: u64,
) -> Result<(), String> {
    if new.height < resolved_height || new.height >= resolved_height + ROW_PAD {
        return Err(format!(
            "it reads {} rows, not the {resolved_height} asked",
            new.height
        ));
    }
    if new.height <= old.height {
        return Err(format!(
            "its {} rows are no more than the cached {}",
            new.height, old.height
        ));
    }
    if new.duration_ms.abs_diff(audio_ms) > DURATION_SLACK_MS {
        return Err(format!(
            "it is {} ms long, the audio {audio_ms} ms",
            new.duration_ms
        ));
    }
    if !new.end_decoded {
        return Err(format!(
            "no picture {NEAR_END_MS} ms before its end decoded"
        ));
    }
    let shift_us = new.first_ms.abs_diff(old.first_ms) * 1000;
    if shift_us > new.frame_us {
        return Err(format!(
            "its first picture is at {} ms, the cached one's at {} ms",
            new.first_ms, old.first_ms
        ));
    }
    Ok(())
}

/// A song's cached files: the lowest row of the video that has both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cached {
    pub row_id: i64,
    pub video: PathBuf,
    pub audio: PathBuf,
}

/// The cached files of `youtube_id`, `None` when no row of it is downloaded.
pub(crate) async fn cached(
    pool: &SqlitePool,
    youtube_id: &str,
) -> Result<Option<Cached>, sqlx::Error> {
    let row: Option<(i64, String, String)> = sqlx::query_as(
        "SELECT id, file_path, audio_file_path FROM videos \
         WHERE youtube_id = ? AND normalized = 1 \
           AND file_path IS NOT NULL AND audio_file_path IS NOT NULL \
         ORDER BY id LIMIT 1",
    )
    .bind(youtube_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(row_id, video, audio)| Cached {
        row_id,
        video: PathBuf::from(video),
        audio: PathBuf::from(audio),
    }))
}

/// Record a check of `youtube_id`'s video on every row of it (V35). `cap` =
/// the cap it was checked at; `None` keeps the stored one, so a check that
/// did not finish is checked again.
pub(crate) async fn record(
    pool: &SqlitePool,
    youtube_id: &str,
    cap: Option<u32>,
    state: &str,
    at_ms: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE videos SET video_upgrade_cap = COALESCE(?, video_upgrade_cap), \
         video_upgrade_state = ?, video_upgrade_at = ? WHERE youtube_id = ?",
    )
    .bind(cap.map(i64::from))
    .bind(state)
    .bind(at_ms)
    .bind(youtube_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// The temp a song's upgrade downloads into (the startup sweep's
/// `TEMP_RE` removes one a crash left).
pub(crate) fn upgrade_temp(cache_dir: &Path, youtube_id: &str) -> PathBuf {
    cache_dir.join(format!("{youtube_id}_video_upgrade_temp.mp4"))
}

/// The steps of one upgrade that run yt-dlp and the readers.
pub(crate) trait Steps {
    /// What a download of `youtube_id` at `cap` would fetch now.
    async fn resolve(&self, youtube_id: &str, cap: u32) -> Result<DownloadedFormat, String>;
    /// The facts of the video file at `path` ([`facts_of`] through the real
    /// reader).
    async fn facts(&self, path: &Path) -> Result<VideoFacts, String>;
    /// Download the stream `format_id` of `youtube_id` into `out`; what
    /// yt-dlp said it fetched.
    async fn download(
        &self,
        youtube_id: &str,
        format_id: &str,
        out: &Path,
    ) -> Result<Option<DownloadedFormat>, String>;
    /// The length of the audio file at `path`.
    async fn audio_ms(&self, path: &Path) -> Result<u64, String>;
}

/// How one upgrade ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    /// The taller video is in place.
    Upgraded,
    /// YouTube serves nothing taller at the cap.
    NoBetter,
    /// A player holds the file: nothing changed, try again later.
    Busy,
    /// #223 S12: the downloaded video failed the check ([`verify`]):
    /// nothing changed, and the same stream would fail the same way, so the
    /// check is settled at its cap.
    Refused,
    /// Something failed on the way (a step, the rows): nothing changed, try
    /// again later.
    Failed,
}

impl Outcome {
    /// The V35 state: `refused: <why>` / `failed: <why>` for those.
    pub(crate) fn state(self, error: Option<&str>) -> String {
        let why = error.unwrap_or("unknown");
        match self {
            Outcome::Upgraded => "upgraded".to_string(),
            Outcome::NoBetter => "no_better".to_string(),
            Outcome::Busy => "busy".to_string(),
            Outcome::Refused => format!("refused: {why}"),
            Outcome::Failed => format!("failed: {why}"),
        }
    }

    /// Whether the check is done at its cap (the worker picks the song again
    /// only once the cap rises); a busy or failed one is checked again.
    pub(crate) fn settled(self) -> bool {
        matches!(
            self,
            Outcome::Upgraded | Outcome::NoBetter | Outcome::Refused
        )
    }
}

/// The answer of one upgrade (the route's body).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct UpgradeReport {
    pub youtube_id: String,
    pub cap: u32,
    pub outcome: Outcome,
    /// The cached video before.
    pub old: Option<VideoFacts>,
    /// What a download at the cap fetches now.
    pub resolved: Option<DownloadedFormat>,
    /// The downloaded video, once read.
    pub new: Option<VideoFacts>,
    pub error: Option<String>,
}

impl UpgradeReport {
    fn new(youtube_id: &str, cap: u32) -> Self {
        Self {
            youtube_id: youtube_id.to_string(),
            cap,
            outcome: Outcome::Failed,
            old: None,
            resolved: None,
            new: None,
            error: None,
        }
    }

    fn ended(mut self, outcome: Outcome, error: Option<String>) -> Self {
        self.outcome = outcome;
        self.error = error;
        self
    }
}

/// Upgrade `youtube_id`'s cached video at `cap` (module doc), recording the
/// check at `now_ms`. Every way out leaves the song playable: the temp is
/// removed unless it became the video.
pub(crate) async fn run<S: Steps>(
    pool: &SqlitePool,
    cache_dir: &Path,
    youtube_id: &str,
    cap: u32,
    steps: &S,
    now_ms: i64,
) -> UpgradeReport {
    let temp = upgrade_temp(cache_dir, youtube_id);
    let report = attempt(pool, youtube_id, cap, steps, &temp).await;
    if report.outcome != Outcome::Upgraded {
        let _ = std::fs::remove_file(&temp);
    }
    let state = report.outcome.state(report.error.as_deref());
    let cap_checked = report.outcome.settled().then_some(cap);
    if let Err(e) = record(pool, youtube_id, cap_checked, &state, now_ms).await {
        tracing::warn!(youtube_id, "video upgrade: the check was not recorded: {e}");
    }
    report
}

async fn attempt<S: Steps>(
    pool: &SqlitePool,
    youtube_id: &str,
    cap: u32,
    steps: &S,
    temp: &Path,
) -> UpgradeReport {
    let mut report = UpgradeReport::new(youtube_id, cap);
    let song = match cached(pool, youtube_id).await {
        Ok(Some(song)) => song,
        Ok(None) => return report.ended(Outcome::Failed, Some("not downloaded".into())),
        Err(e) => return report.ended(Outcome::Failed, Some(format!("the rows: {e}"))),
    };
    let old = match steps.facts(&song.video).await {
        Ok(old) => old,
        Err(e) => return report.ended(Outcome::Failed, Some(format!("the cached video: {e}"))),
    };
    report.old = Some(old);
    let resolved = match steps.resolve(youtube_id, cap).await {
        Ok(resolved) => resolved,
        Err(e) => return report.ended(Outcome::Failed, Some(format!("the resolve: {e}"))),
    };
    report.resolved = Some(resolved.clone());
    let Some(height) = resolved.height.filter(|_| better(&resolved, &old)) else {
        return report.ended(Outcome::NoBetter, None);
    };
    let _ = std::fs::remove_file(temp);
    let fetched = match steps.download(youtube_id, &resolved.format_id, temp).await {
        Ok(fetched) => fetched,
        Err(e) => return report.ended(Outcome::Failed, Some(format!("the download: {e}"))),
    };
    let new = match steps.facts(temp).await {
        Ok(new) => new,
        Err(e) => return report.ended(Outcome::Failed, Some(format!("the new video: {e}"))),
    };
    report.new = Some(new);
    let audio_ms = match steps.audio_ms(&song.audio).await {
        Ok(ms) => ms,
        Err(e) => return report.ended(Outcome::Failed, Some(format!("the audio: {e}"))),
    };
    if let Err(why) = verify(height, &old, &new, audio_ms) {
        return report.ended(Outcome::Refused, Some(why));
    }
    match swap::swap(pool, youtube_id, &song.video, temp).await {
        swap::Swapped::Done => {}
        swap::Swapped::Busy(why) => return report.ended(Outcome::Busy, Some(why)),
        swap::Swapped::Refused(why) => return report.ended(Outcome::Failed, Some(why)),
    }
    let format = fetched.unwrap_or(resolved);
    if let Err(e) = format::record(pool, song.row_id, Some(&format)).await {
        tracing::warn!(
            youtube_id,
            "video upgrade: the new format was not recorded: {e}"
        );
    }
    report.ended(Outcome::Upgraded, None)
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
