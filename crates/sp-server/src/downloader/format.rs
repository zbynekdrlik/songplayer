//! #223 D8 (S9a): yt-dlp's video format selector and its height cap, the
//! `max_resolution` setting, read at every download.
//!
//! yt-dlp's `/` takes the FIRST alternative that matches any format; one
//! alternative is never weighed against the next. So the selector walks the
//! resolution tiers from the cap down (2160, 1440, 1080, 720 — those at or
//! under the cap) and, within a tier, tries AV1 / VP9 over HTTPS (the DASH
//! path Media Foundation plays; yt-dlp's default sort puts resolution first,
//! then av01 before vp9) before H.264 over HLS. H.264 comes only via HLS: a
//! 1080p H.264 DASH encode (THE DEEP, `xrhVLX6vwPk`) returns EOS in MF's
//! hardware transform. Then the same two with no lower bound, then any SDR,
//! then anything under the cap. SDR wherever it can: the reader is NV12
//! 8-bit. Box check (#223 comment 6099757719): D8's untiered selector picked
//! THE DEEP at 360p, its only VP9, instead of H.264 1080p.

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

#[cfg(test)]
#[path = "format_tests.rs"]
mod tests;
