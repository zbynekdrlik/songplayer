//! #223 D8 (S9a): yt-dlp's video format selector and its height cap, the
//! `max_resolution` setting, read at every download.
//!
//! yt-dlp's `/` takes the FIRST alternative that matches any format; one
//! alternative is never weighed against the next. So the selector walks the
//! resolution tiers from the cap down (2160, 1440, 1080, 720 — those at or
//! under the cap) and, within a tier, tries AV1 / VP9 over HTTPS (the DASH
//! path Media Foundation plays; yt-dlp's default sort puts resolution first,
//! then av01 before vp9) before H.264 over HLS. H.264 is taken over HLS
//! first: a 1080p H.264 DASH encode (THE DEEP, `xrhVLX6vwPk`) returns EOS in
//! MF's hardware transform. Then the same two with no lower bound, then any
//! SDR, then anything under the cap (the last two can take H.264 DASH when a
//! video has nothing else). SDR wherever it can: the reader is NV12 8-bit.
//! Box check (#223 comment 6099757719): D8's untiered selector picked THE
//! DEEP at 360p, its only VP9, instead of H.264 1080p over HLS. The tiers
//! lean on YouTube serving HLS (it did on 10.10.2026, yt-dlp 2026.08.19): a
//! video with a low AV1 / VP9 and its high rows only as H.264 DASH, and no
//! HLS, still lands at the low AV1 / VP9 (`dash("")` comes before the plain
//! height fallbacks) — deliberately, since that H.264 DASH may stop early.

use sp_core::config::{DEFAULT_MAX_RESOLUTION, SETTING_MAX_RESOLUTION};

/// The lowest cap a download takes.
pub const MIN_RESOLUTION: u32 = 480;
/// The highest cap a download takes (D8).
pub const MAX_RESOLUTION: u32 = 2160;

/// The resolution tiers the selector walks down from the cap.
const TIERS: [u32; 4] = [2160, 1440, 1080, 720];

/// The `-f` selector of a video download capped at `cap` rows (module doc).
pub(crate) fn format_spec(cap: u32) -> String {
    let dash = |floor: &str| {
        format!("bv*[height<={cap}]{floor}[dynamic_range=SDR][protocol=https][vcodec!^=avc1]")
    };
    let hls = |floor: &str| {
        format!("bv*[height<={cap}]{floor}[dynamic_range=SDR][protocol*=m3u8][vcodec^=avc1]")
    };
    let mut alternatives = Vec::new();
    for tier in TIERS.into_iter().filter(|&tier| tier <= cap) {
        let floor = format!("[height>={tier}]");
        alternatives.push(dash(&floor));
        alternatives.push(hls(&floor));
    }
    alternatives.push(dash(""));
    alternatives.push(hls(""));
    alternatives.push(format!("bv*[height<={cap}][dynamic_range=SDR]"));
    alternatives.push(format!("bv*[height<={cap}]"));
    alternatives.join("/")
}

/// The cap of a download from the stored `max_resolution`: a whole number,
/// clamped to [`MIN_RESOLUTION`]..=[`MAX_RESOLUTION`]; unset or unreadable
/// is [`DEFAULT_MAX_RESOLUTION`].
pub(crate) fn max_resolution(raw: Option<&str>) -> u32 {
    raw.and_then(|value| value.trim().parse::<u32>().ok())
        .map_or(DEFAULT_MAX_RESOLUTION, |height| {
            height.clamp(MIN_RESOLUTION, MAX_RESOLUTION)
        })
}

/// A settings PATCH value of `key`: `max_resolution` takes "" (back to the
/// default) or a whole number from [`MIN_RESOLUTION`] to [`MAX_RESOLUTION`],
/// stored trimmed; anything else is refused and nothing is written. Every
/// other key passes unchanged.
pub(crate) fn checked(key: &str, value: &str) -> Result<String, String> {
    if key != SETTING_MAX_RESOLUTION {
        return Ok(value.to_string());
    }
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    match trimmed.parse::<u32>() {
        Ok(height) if (MIN_RESOLUTION..=MAX_RESOLUTION).contains(&height) => Ok(height.to_string()),
        _ => Err(format!(
            "{SETTING_MAX_RESOLUTION} must be a whole number of pixels from {MIN_RESOLUTION} to \
             {MAX_RESOLUTION}, or empty for the default {DEFAULT_MAX_RESOLUTION}"
        )),
    }
}

/// #223 S9b: what yt-dlp prints once the video stream is in place (its
/// `--print`, a later stage than the download, so it downloads as before): a
/// marked line the worker finds among the progress lines.
pub(crate) const FORMAT_PRINT: &str =
    "after_move:SPFMT|%(format_id)s|%(vcodec)s|%(width)s|%(height)s|%(fps)s";

/// The video stream a download really fetched (V34 columns; `None` =
/// yt-dlp did not know it).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DownloadedFormat {
    pub format_id: String,
    pub codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
}

/// The [`FORMAT_PRINT`] line in yt-dlp's stdout (its last one), or `None`
/// when there is none (an older yt-dlp, a print that failed): a field yt-dlp
/// does not know reads `NA`.
pub(crate) fn parse_downloaded_format(stdout: &str) -> Option<DownloadedFormat> {
    let line = stdout
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix("SPFMT|"))?;
    let fields: Vec<&str> = line.split('|').collect();
    let [format_id, codec, width, height, fps] = fields.as_slice() else {
        return None;
    };
    let known = |field: &str| (field != "NA" && !field.is_empty()).then(|| field.to_string());
    Some(DownloadedFormat {
        format_id: known(format_id)?,
        codec: known(codec),
        width: width.parse().ok(),
        height: height.parse().ok(),
        fps: fps.parse().ok(),
    })
}

/// Record the format of the files just recorded for the video `video_id` (a
/// row id) belongs to, on every row of it (the files are the video's, shared
/// by its rows). `None` = not known (no format line, or a peer's copy): every
/// column NULL, so no row keeps the format of files that were replaced.
pub(crate) async fn record(
    pool: &sqlx::SqlitePool,
    video_id: i64,
    format: Option<&DownloadedFormat>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE videos SET video_format_id = ?, video_codec = ?, video_width = ?, \
         video_height = ?, video_fps = ? \
         WHERE youtube_id = (SELECT youtube_id FROM videos WHERE id = ?)",
    )
    .bind(format.map(|f| f.format_id.as_str()))
    .bind(format.and_then(|f| f.codec.as_deref()))
    .bind(format.and_then(|f| f.width).map(i64::from))
    .bind(format.and_then(|f| f.height).map(i64::from))
    .bind(format.and_then(|f| f.fps))
    .bind(video_id)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
#[path = "format_tests.rs"]
mod tests;
