//! #223 D8 (S9a): yt-dlp's video format selector and its height cap, the
//! `max_resolution` setting, read at every download.
//!
//! yt-dlp's `/` takes the FIRST alternative that matches any format; one
//! alternative is never weighed against the next. So the selector walks the
//! resolution tiers from the cap down (1440, 1080, 720 — those at or under
//! the cap; S10b: above 1440 rows first, below) and, within a tier, tries
//! AV1 / VP9 over HTTPS (the DASH
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
//!
//! #223 S10b (comment 6102705701): at a cap above 1440 the first two
//! alternatives take a picture taller than 1440 rows at
//! [`TALL_MAX_FPS`] or less (strict: no known rate, not taken), and every
//! later one is capped at [`ANY_FPS_MAX_HEIGHT`]. NVDEC decodes a 4K
//! picture with its readback in 17.7–19.6 ms whatever its rate: 42–49 % of
//! a 24/25 fps period, over D2's 50 % from 30 fps on, beyond real time at
//! 50/60. Each alternative's ceiling is the cap, so a bound on a 2160 tier
//! alone would let 4K60 through the 1440 tier. A 4K30/60 video lands at
//! 1440 rows, as before S10b.

use sp_core::config::{
    DEFAULT_MAX_RESOLUTION, DEFAULT_MAX_RESOLUTION_SOFTWARE, SETTING_MAX_RESOLUTION,
    SETTING_VIDEO_HW_DECODE, video_hw_decode,
};

/// The lowest cap a download takes.
pub const MIN_RESOLUTION: u32 = 480;
/// The highest cap a download takes (D8).
pub const MAX_RESOLUTION: u32 = 2160;

/// The resolution tiers the selector walks down from the cap.
const TIERS: [u32; 3] = [1440, 1080, 720];

/// #223 S10b: the tallest picture taken at any frame rate (module doc).
pub const ANY_FPS_MAX_HEIGHT: u32 = 1440;
/// #223 S10b: the highest frame rate of a picture taller than
/// [`ANY_FPS_MAX_HEIGHT`] (module doc).
pub const TALL_MAX_FPS: u32 = 25;

/// The `-f` selector of a video download capped at `cap` rows (module doc).
pub(crate) fn format_spec(cap: u32) -> String {
    let dash = |top: u32, floor: &str| {
        format!("bv*[height<={top}]{floor}[dynamic_range=SDR][protocol=https][vcodec!^=avc1]")
    };
    let hls = |top: u32, floor: &str| {
        format!("bv*[height<={top}]{floor}[dynamic_range=SDR][protocol*=m3u8][vcodec^=avc1]")
    };
    let mut alternatives = Vec::new();
    if cap > ANY_FPS_MAX_HEIGHT {
        let tall = format!("[height>{ANY_FPS_MAX_HEIGHT}][fps<={TALL_MAX_FPS}]");
        alternatives.push(dash(cap, &tall));
        alternatives.push(hls(cap, &tall));
    }
    let top = cap.min(ANY_FPS_MAX_HEIGHT);
    for tier in TIERS.into_iter().filter(|&tier| tier <= top) {
        let floor = format!("[height>={tier}]");
        alternatives.push(dash(top, &floor));
        alternatives.push(hls(top, &floor));
    }
    alternatives.push(dash(top, ""));
    alternatives.push(hls(top, ""));
    alternatives.push(format!("bv*[height<={top}][dynamic_range=SDR]"));
    alternatives.push(format!("bv*[height<={top}]"));
    alternatives.join("/")
}

/// The cap of a download from the stored `max_resolution`: a whole number,
/// clamped to [`MIN_RESOLUTION`]..=[`MAX_RESOLUTION`]; unset or unreadable
/// is [`DEFAULT_MAX_RESOLUTION`] while hardware video decode is on, else
/// [`DEFAULT_MAX_RESOLUTION_SOFTWARE`] (#223 S10b, comment 6102693285).
pub(crate) fn max_resolution(raw: Option<&str>, hw_decode: bool) -> u32 {
    let default = if hw_decode {
        DEFAULT_MAX_RESOLUTION
    } else {
        DEFAULT_MAX_RESOLUTION_SOFTWARE
    };
    raw.and_then(|value| value.trim().parse::<u32>().ok())
        .map_or(default, |height| {
            height.clamp(MIN_RESOLUTION, MAX_RESOLUTION)
        })
}

/// The cap a download takes now: the stored `max_resolution` and
/// `video_hw_decode` ([`max_resolution`]). A setting that cannot be read
/// counts as unset (hardware decode then off, the producer's own rule).
/// The download and the YouTube probe both read it here.
pub(crate) async fn live_cap(pool: &sqlx::SqlitePool) -> u32 {
    let stored = crate::db::models::get_setting(pool, SETTING_MAX_RESOLUTION).await;
    let hw = crate::db::models::get_setting(pool, SETTING_VIDEO_HW_DECODE).await;
    max_resolution(
        stored.ok().flatten().as_deref(),
        video_hw_decode(hw.ok().flatten().as_deref()),
    )
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
             {MAX_RESOLUTION}, or empty for the default ({DEFAULT_MAX_RESOLUTION} with hardware \
             video decode, else {DEFAULT_MAX_RESOLUTION_SOFTWARE})"
        )),
    }
}

/// The marked line naming a video stream (`parse_downloaded_format` reads it).
macro_rules! format_fields {
    () => {
        "SPFMT|%(format_id)s|%(vcodec)s|%(width)s|%(height)s|%(fps)s"
    };
}

/// #223 S9b: what yt-dlp prints once the video stream is in place (its
/// `--print`, a later stage than the download, so it downloads as before): a
/// marked line the worker finds among the progress lines.
pub(crate) const FORMAT_PRINT: &str = concat!("after_move:", format_fields!());

/// #232: the same line at yt-dlp's video stage, which only resolves the
/// format (`--print` there simulates): the YouTube probe's
/// (`downloader::probe`).
pub(crate) const FORMAT_PROBE_PRINT: &str = format_fields!();

/// The video stream a download really fetched (V34 columns; `None` =
/// yt-dlp did not know it).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
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
