//! Resolume title show/hide with parallel multi-clip opacity fade.

use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use tracing::debug;

use crate::resolume::TITLE_TOKEN;
use crate::resolume::driver::{ClipInfo, HostDriver};

const TEXT_SETTLE_MS: u64 = 35;
const FADE_DURATION_MS: u64 = 1000;
const FADE_STEPS: u32 = 20;

/// Format title text matching legacy Python behavior — clean `Song - Artist`.
/// No warning indicator (gemini_failed is no longer surfaced in titles).
pub fn format_title_text(song: &str, artist: &str) -> String {
    match (song.is_empty(), artist.is_empty()) {
        (false, false) => format!("{song} - {artist}"),
        (false, true) => song.to_string(),
        (true, false) => artist.to_string(),
        (true, true) => String::new(),
    }
}

/// Generate `n` evenly-spaced opacity values from `step/n` to `1.0`.
pub fn fade_steps(n: u32) -> Vec<f64> {
    (1..=n).map(|i| i as f64 / n as f64).collect()
}

/// Show title across all `#sp-title` clips in parallel.
pub async fn show_title(
    driver: &mut HostDriver,
    song: &str,
    artist: &str,
) -> Result<(), anyhow::Error> {
    let clips: Vec<ClipInfo> = match driver.clip_mapping.get(TITLE_TOKEN) {
        Some(v) if !v.is_empty() => v.clone(),
        _ => {
            debug!(
                token = TITLE_TOKEN,
                "no Resolume clips found, skipping show_title"
            );
            return Ok(());
        }
    };

    let text = format_title_text(song, artist);
    if text.is_empty() {
        return Ok(());
    }

    driver.ensure_endpoint().await?;
    let driver_ref: &HostDriver = driver;

    set_text_all(driver_ref, &clips, &text).await?;
    debug!(
        token = TITLE_TOKEN,
        count = clips.len(),
        %text,
        "set title text on all clips"
    );

    tokio::time::sleep(Duration::from_millis(TEXT_SETTLE_MS)).await;

    let step_delay = Duration::from_millis(FADE_DURATION_MS / FADE_STEPS as u64);
    for opacity in fade_steps(FADE_STEPS) {
        set_opacity_all(driver_ref, &clips, opacity).await?;
        tokio::time::sleep(step_delay).await;
    }

    debug!(
        token = TITLE_TOKEN,
        count = clips.len(),
        "title fade-in complete"
    );
    Ok(())
}

/// Hide title across all `#sp-title` clips in parallel.
pub async fn hide_title(driver: &mut HostDriver) -> Result<(), anyhow::Error> {
    let clips: Vec<ClipInfo> = match driver.clip_mapping.get(TITLE_TOKEN) {
        Some(v) if !v.is_empty() => v.clone(),
        _ => {
            debug!(
                token = TITLE_TOKEN,
                "no Resolume clips found, skipping hide_title"
            );
            return Ok(());
        }
    };

    driver.ensure_endpoint().await?;
    let driver_ref: &HostDriver = driver;

    let step_delay = Duration::from_millis(FADE_DURATION_MS / FADE_STEPS as u64);
    let steps: Vec<f64> = fade_steps(FADE_STEPS);
    for opacity in steps.iter().rev() {
        set_opacity_all(driver_ref, &clips, *opacity).await?;
        tokio::time::sleep(step_delay).await;
    }
    set_opacity_all(driver_ref, &clips, 0.0).await?;

    set_text_all(driver_ref, &clips, "").await?;

    debug!(
        token = TITLE_TOKEN,
        count = clips.len(),
        "title fade-out complete"
    );
    Ok(())
}

async fn set_text_all(
    driver: &HostDriver,
    clips: &[ClipInfo],
    text: &str,
) -> Result<(), anyhow::Error> {
    let mut futs = FuturesUnordered::new();
    for clip in clips {
        futs.push(driver.set_text(clip.text_param_id, text));
    }
    while let Some(res) = futs.next().await {
        res?;
    }
    Ok(())
}

async fn set_opacity_all(
    driver: &HostDriver,
    clips: &[ClipInfo],
    opacity: f64,
) -> Result<(), anyhow::Error> {
    let mut futs = FuturesUnordered::new();
    for clip in clips {
        futs.push(driver.set_clip_opacity(clip.clip_id, opacity));
    }
    while let Some(res) = futs.next().await {
        res?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_title_text_song_and_artist() {
        assert_eq!(format_title_text("My Song", "Artist"), "My Song - Artist");
    }

    #[test]
    fn format_title_text_song_only() {
        assert_eq!(format_title_text("My Song", ""), "My Song");
    }

    #[test]
    fn format_title_text_artist_only() {
        assert_eq!(format_title_text("", "Artist"), "Artist");
    }

    #[test]
    fn format_title_text_empty() {
        assert_eq!(format_title_text("", ""), "");
    }

    #[test]
    fn format_title_text_no_warning_indicator_anywhere() {
        let result = format_title_text("Song", "Artist");
        assert!(!result.contains('\u{26A0}'));
        assert!(!result.contains('⚠'));
    }

    #[test]
    fn fade_steps_20_steps_over_1s() {
        let steps = fade_steps(20);
        assert_eq!(steps.len(), 20);
        assert!((steps[0] - 0.05).abs() < 0.001);
        assert!((steps[19] - 1.0).abs() < 0.001);
    }

    #[test]
    fn fade_steps_values_are_monotonically_increasing() {
        let steps = fade_steps(20);
        for i in 1..steps.len() {
            assert!(steps[i] > steps[i - 1]);
        }
    }
}
